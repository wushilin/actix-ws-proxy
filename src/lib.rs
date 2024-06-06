use std::error::Error;

use actix::{
    io::{SinkWrite, WriteHandler},
    Actor, ActorContext, AsyncContext, StreamHandler,
};
use actix_web::{
    error::{InternalError, PayloadError},
    http::StatusCode,
    HttpRequest, HttpResponse,
};
use actix_web_actors::ws::{self, handshake, CloseReason, ProtocolError, WebsocketContext};
use bytes::Bytes;
use futures::{Sink, Stream, StreamExt};

/// WebsocketProxy proxies an incoming websocket connection to another websocket, connected via awc.
pub struct WebsocketProxy<S>
where
    S: Unpin + Sink<ws::Message>,
{
    send: SinkWrite<ws::Message, S>,
}

impl<S> WebsocketProxy<S>
where
    S: Unpin + Sink<ws::Message> + 'static,
{
    fn error<E>(&mut self, err: E, ctx: &mut <Self as Actor>::Context)
    where
        E: Error,
    {
        let reason = Some(CloseReason {
            code: ws::CloseCode::Error,
            description: Some(err.to_string()),
        });

        ctx.close(reason.clone());
        let _ = self.send.write(ws::Message::Close(reason)); // if we can't send an error message, so it goes
        self.send.close();

        ctx.stop();
    }
}

/// start a websocket proxy
///
/// `target` should be a URL of the form `ws://<host>` or `wss://<host>`
/// see awc::Client::ws for more information
/// req and stream are exactly like the arguments to actix_web_actors::ws::start
/// ```
/// # use actix_web::{get, Error, HttpRequest, HttpResponse, web};
/// #[get("/proxy/{port}")]
/// async fn proxy(
///     req: HttpRequest,
///     stream: web::Payload,
///     port: web::Path<u16>,
/// ) -> Result<HttpResponse, Error> {
///     actix_ws_proxy::start(&req, format!("ws://127.0.0.1:{}", port), stream).await
/// }
/// ```
pub async fn start<T>(
    req: &HttpRequest,
    target: String,
    stream: T,
) -> Result<HttpResponse, actix_web::Error>
where
    T: Stream<Item = Result<Bytes, PayloadError>> + 'static,
{
    let result = start_ex(req, target, None, stream).await;
    return result;
}

pub async fn start_ex<T>(
    req: &HttpRequest,
    target: String,
    host: Option<String>,
    stream: T,
) -> Result<HttpResponse, actix_web::Error>
where
    T: Stream<Item = Result<Bytes, PayloadError>> + 'static,
{
    let mut res = handshake(req)?;

    match host {
        None => {
            let (_, conn) = awc::Client::new()
                .ws(target)
                .connect()
                .await
                .map_err(|e| InternalError::new(e, StatusCode::BAD_GATEWAY))?;

            let (send, recv) = conn.split();

            let out = WebsocketContext::with_factory(stream, |ctx| {
                ctx.add_stream(recv);
                WebsocketProxy {
                    send: SinkWrite::new(send, ctx),
                }
            });

            Ok(res.streaming(out))
        },
        Some(host) => {
            let (_, conn) = awc::Client::new()
                .ws(target)
                .header("host", host)
                .connect() 
                .await
                .map_err(|e| InternalError::new(e, StatusCode::BAD_GATEWAY))?;

            let (send, recv) = conn.split();

            let out = WebsocketContext::with_factory(stream, |ctx| {
                ctx.add_stream(recv);
                WebsocketProxy {
                    send: SinkWrite::new(send, ctx),
                }
            });

            Ok(res.streaming(out))
        }
    }
}

impl<S> WriteHandler<ProtocolError> for WebsocketProxy<S>
where
    S: Unpin + 'static + Sink<ws::Message>,
{
    fn error(&mut self, err: ProtocolError, ctx: &mut Self::Context) -> actix::Running {
        self.error(err, ctx);
        actix::Running::Stop
    }
}

impl<S> Actor for WebsocketProxy<S>
where
    S: Unpin + 'static + Sink<ws::Message>,
{
    type Context = WebsocketContext<Self>;
}

// This represents messages from upstream, so we send them downstream
impl<S> StreamHandler<Result<ws::Frame, ProtocolError>> for WebsocketProxy<S>
where
    S: Unpin + Sink<ws::Message> + 'static,
{
    fn handle(&mut self, item: Result<ws::Frame, ProtocolError>, ctx: &mut Self::Context) {
        let frame = match item {
            Ok(frame) => frame,
            Err(err) => return self.error(err, ctx),
        };
        let msg = match frame {
            ws::Frame::Text(t) => match t.try_into() {
                Ok(t) => ws::Message::Text(t),
                Err(e) => {
                    self.error(e, ctx);
                    return;
                }
            },
            ws::Frame::Binary(b) => ws::Message::Binary(b),
            ws::Frame::Continuation(c) => ws::Message::Continuation(c),
            ws::Frame::Ping(p) => ws::Message::Ping(p),
            ws::Frame::Pong(p) => ws::Message::Pong(p),
            ws::Frame::Close(r) => ws::Message::Close(r),
        };

        ctx.write_raw(msg)
    }
}

// This represents messages from downstream, so they are sent upstream
impl<S> StreamHandler<Result<ws::Message, ProtocolError>> for WebsocketProxy<S>
where
    S: Unpin + Sink<ws::Message> + 'static,
{
    fn handle(&mut self, item: Result<ws::Message, ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => return self.error(err, ctx),
        };

        // if this fails we're probably shutting down
        let _ = self.send.write(msg);
    }
}

#[cfg(test)]
mod tests {
    use actix::{Actor, StreamHandler};
    use actix_web::{
        get,
        web::{self, Path},
        App, Error, HttpRequest, HttpResponse, HttpServer,
    };
    use actix_web_actors::ws;
    use futures::{SinkExt, StreamExt};

    #[derive(Debug)]
    struct TestActor;

    impl Actor for TestActor {
        type Context = ws::WebsocketContext<Self>;
    }

    impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for TestActor {
        fn handle(
            &mut self,
            item: Result<ws::Message, ws::ProtocolError>,
            ctx: &mut Self::Context,
        ) {
            match item.unwrap() {
                ws::Message::Text(txt) => ctx.text(txt),
                ws::Message::Binary(bin) => ctx.binary(bin),
                ws::Message::Ping(arg) => ctx.pong(&arg),
                ws::Message::Close(reason) => ctx.close(reason),
                _ => (),
            }
        }
    }

    #[get("/proxy/{port}")]
    async fn proxy(
        req: HttpRequest,
        stream: web::Payload,
        port: Path<u16>,
    ) -> Result<HttpResponse, Error> {
        crate::start(&req, format!("ws://127.0.0.1:{}", port), stream).await
    }

    #[get("/")]
    async fn index(req: HttpRequest, stream: web::Payload) -> Result<HttpResponse, Error> {
        ws::start(TestActor, &req, stream)
    }

    macro_rules! get_server {
        ($factory:expr) => {{
            let port = portpicker::pick_unused_port().expect("No ports free");
            let server = HttpServer::new(|| App::new().service($factory))
                .bind(("0.0.0.0", port))
                .expect("couldn't start server")
                .run();

            (server, port)
        }};
    }

    #[actix::test]
    async fn with_proxy() {
        let (srv, port) = get_server!(index);
        actix::spawn(srv);
        let (proxysrv, proxyport) = get_server!(proxy);
        actix::spawn(proxysrv);

        let client = awc::Client::new();
        let (_resp, mut conn) = client
            .ws(format!("ws://localhost:{}/proxy/{}", proxyport, port))
            .connect()
            .await
            .unwrap();

        conn.send(ws::Message::Text("echo.into".into()))
            .await
            .unwrap();

        let resp = conn.next().await.unwrap().unwrap();
        assert_eq!(resp, ws::Frame::Text("echo.into".into()));

        let bytes = bytes::Bytes::from_static(&[0x11, 0x22, 0x33, 0x55]);

        conn.send(awc::ws::Message::Binary(bytes.clone()))
            .await
            .unwrap();

        let resp = conn.next().await.unwrap().unwrap();
        assert_eq!(resp, ws::Frame::Binary(bytes.clone()));

        conn.send(ws::Message::Ping(bytes.clone())).await.unwrap();

        let resp = conn.next().await.unwrap().unwrap();
        assert_eq!(resp, ws::Frame::Pong(bytes.clone()));
    }
}
