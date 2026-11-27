//! Reaxt の `App` を RPoem の `Route` に載せるアダプタ。
//!
//! RPoem のルーターはセグメント数が一致するパターンしか照合できず(ワイルドカード・フォールバック無し)、
//! Reaxt の `*slug`(残り全部)は深さ 1..=MAX_CATCH_ALL_DEPTH の `:a1/:a2/...` に展開して登録する。
//! 実際の描画は常に要求の実パスを `App::handle` に渡すので、パラメータ名の違いは影響しない。
use http_body_util::{BodyExt, Limited};
use open_runo_poem_compat::{get, handler_fn, hyper_compat, post, Route};
use reaxt::app::{App, Request};
use std::sync::Arc;

/// `*slug` を展開する最大の深さ。
pub const MAX_CATCH_ALL_DEPTH: usize = 8;

/// Reaxt パターンを RPoem パターンの一覧へ展開する(純関数)。
pub fn expand(pattern: &str) -> Vec<String> {
    match pattern.find("/*") {
        None => vec![pattern.to_string()],
        Some(i) => {
            let prefix = &pattern[..i];
            (1..=MAX_CATCH_ALL_DEPTH)
                .map(|d| {
                    let tail: Vec<String> = (1..=d).map(|k| format!(":a{k}")).collect();
                    format!("{prefix}/{}", tail.join("/"))
                })
                .collect()
        }
    }
}

/// `App` を載せた RPoem `Route` を返す(`Server::new(..).run(route)` や `nest` にそのまま渡せる)。
pub fn route(app: Arc<App>) -> Route {
    let mut r = Route::new();
    // ページ(GET)
    for pat in app.patterns() {
        for rp in expand(&pat) {
            r = r.at(&rp, get(host_handler(app.clone())));
        }
    }
    // 静的アセット(GET)
    for p in app.asset_paths() {
        r = r.at(&p, get(host_handler(app.clone())));
    }
    // Connectメソッド(POST)。同じパスにページがあっても別メソッドとして登録される。
    for p in app.method_paths() {
        r = r.at(&p, post(host_handler(app.clone())));
    }
    // 外部連携の入口(POST。Cosmo向けサブグラフなど)と、機械可読の一覧(GET)
    for p in app.gateway_paths() {
        r = r.at(&p, post(host_handler(app.clone())));
    }
    r = r.at("/_reaxt/manifest.json", get(host_handler(app.clone())));
    // ストリーミングSSRのページ(GET)
    for p in app.stream_paths().into_iter().chain(app.async_stream_paths()) {
        r = r.at(&p, get(host_handler(app.clone())));
    }
    // WebSocket(GET + Upgrade)。接続は RPoem が持ち、イベントごとに App::ws_handle を呼ぶ。
    for rp in app.ws_patterns().iter().flat_map(|p| expand(p)) {
        r = r.at(&rp, get(ws_handler(app.clone())));
    }
    r
}

/// 本番向けの既定: `route` にRPoemのトレース(要求ごとのログ)と圧縮を重ねる。
/// CORSが要る場合は、返った `Route` に `.with_cors()` を足す(同一オリジンのBFF構成では不要)。
pub fn route_production(app: Arc<App>) -> Route {
    route(app).with_tracing().with_compression()
}


/// 複数のチャンクを1つずつ別のフレームとして送るボディ(HTTP/1.1ではchunked転送になる)。
struct Chunks(std::collections::VecDeque<bytes_like::Bytes>);

impl http_body::Body for Chunks {
    type Data = bytes_like::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Ready(self.0.pop_front().map(|b| Ok(http_body::Frame::data(b))))
    }
}

fn chunked_body(parts: Vec<String>) -> open_runo_poem_compat::hyper_compat::Body {
    use http_body_util::BodyExt;
    Chunks(parts.into_iter().map(|s| bytes_like::Bytes::from(s.into_bytes())).collect()).boxed()
}


/// 受信側がチャネルから1つずつ取り出して送るボディ(送り手が閉じたら終わり)。
struct ChannelBody(tokio::sync::mpsc::Receiver<String>);

impl http_body::Body for ChannelBody {
    type Data = bytes_like::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|s| Ok(http_body::Frame::data(bytes_like::Bytes::from(s.into_bytes())))))
    }
}

/// 非同期ストリーミング: シェルを**待たずに最初に送り**、各境界を並行に走らせ、**終わった順**に差し替えを送る。
/// 各境界は期限つき(超えたら `on_timeout` で差し替え)。クライアントが切れたら残りの処理を中止する。
fn async_body(s: reaxt::stream::AsyncStream) -> open_runo_poem_compat::hyper_compat::Body {
    use http_body_util::BodyExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        if tx.send(s.head).await.is_err() {
            return;
        }
        let mut set = tokio::task::JoinSet::new();
        for (i, p) in s.pendings.into_iter().enumerate() {
            let tx = tx.clone();
            set.spawn(async move {
                let node = match tokio::time::timeout(p.timeout, p.fut).await {
                    Ok(v) => v,
                    Err(_) => p.on_timeout,
                };
                let _ = tx.send(reaxt::stream::resolved_chunk(i, &node)).await;
            });
        }
        loop {
            tokio::select! {
                _ = tx.closed() => { set.abort_all(); return; }
                r = set.join_next() => if r.is_none() { break },
            }
        }
        let _ = tx.send(s.tail).await;
    });
    ChannelBody(rx).boxed()
}

/// WebSocket のアップグレードを受け、`App::ws_handle` が返す指示を実行する。
/// 順序: Open → (受信した Text ごとに) → 接続が切れたら Close。`WsOut::Close` が出たらこちらから閉じる。
fn ws_handler(app: Arc<App>) -> open_runo_poem_compat::Handler {
    handler_fn(move |req, _params| {
        let app = app.clone();
        async move {
            let path = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".to_string());
            hyper_compat::websocket_upgrade(req, move |mut conn| {
                Box::pin(async move {
                    use reaxt::app::{WsEvent, WsOut};
                    // 戻り値: true なら続ける
                    async fn run(conn: &mut hyper_compat::WebSocketConnection, outs: Option<Vec<WsOut>>) -> bool {
                        for o in outs.unwrap_or_default() {
                            match o {
                                WsOut::Send(t) => {
                                    if conn.send_text(t).await.is_err() {
                                        return false;
                                    }
                                }
                                WsOut::Close => {
                                    let _ = conn.close().await;
                                    return false;
                                }
                            }
                        }
                        true
                    }
                    if !run(&mut conn, app.ws_handle(&path, &WsEvent::Open)).await {
                        return;
                    }
                    while let Some(m) = conn.recv().await {
                        if let hyper_compat::WsMessage::Text(t) = m {
                            if !run(&mut conn, app.ws_handle(&path, &WsEvent::Text(t))).await {
                                return;
                            }
                        }
                    }
                    let _ = app.ws_handle(&path, &WsEvent::Close);
                })
            })
        }
    })
}

fn host_handler(app: Arc<App>) -> open_runo_poem_compat::Handler {
    handler_fn(move |req, _params| {
        let app = app.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
                .collect();
            // 本文は上限つきで読む(超えたら413)
            let body = match Limited::new(req.into_body(), reaxt::backend::MAX_BODY).collect().await {
                Ok(c) => String::from_utf8_lossy(&c.to_bytes()).into_owned(),
                Err(_) => {
                    return open_runo_poem_compat::empty_status(open_runo_poem_compat::StatusCode::PAYLOAD_TOO_LARGE)
                }
            };
            let res = app.dispatch(&Request { method: &method, path: &path, headers: &headers, body: &body });
            let mut b = hyper::Response::builder()
                .status(res.status)
                .header("content-type", res.content_type)
                .header("x-content-type-options", "nosniff");
            for (k, v) in &res.headers {
                b = b.header(k.as_str(), v.as_str());
            }
            let body = match (res.streaming, res.chunks, res.bytes) {
                (Some(s), _, _) => async_body(s),
                (None, Some(parts), _) => chunked_body(parts),
                (None, None, Some(v)) => hyper_compat::fixed_body(v.into()),
                (None, None, None) => hyper_compat::fixed_body(res.body.into()),
            };
            b.body(body).unwrap_or_else(|_| {
                open_runo_poem_compat::empty_status(open_runo_poem_compat::StatusCode::INTERNAL_SERVER_ERROR)
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_runo_poem_compat::testing::TestServer;
    use open_runo_poem_compat::Method;
    use reaxt::react::VNode;
    use reaxt::RenderMode;

    #[test]
    fn expands_catch_all() {
        assert_eq!(expand("/users/:id"), vec!["/users/:id"]);
        let e = expand("/docs/*p");
        assert_eq!(e.len(), MAX_CATCH_ALL_DEPTH);
        assert_eq!(e[0], "/docs/:a1");
        assert_eq!(e[2], "/docs/:a1/:a2/:a3");
    }

    #[tokio::test]
    async fn connect_post_guard_and_route_rules_over_rpoem() {
        use reaxt::app::RouteRule;
        use reaxt::backend::{Denied, Module};
        let app = Arc::new(
            App::new("be")
                .page("index.rs", RenderMode::Ssr, |_| VNode::element("p").build())
                .route_rule("/", RouteRule::default().cache(30))
                .module(
                    Module::new()
                        .guard(|c| if c.header("x-key") == Some("k") { Ok(()) } else { Err(Denied::new(401, "no key")) })
                        .method("demo.v1.Echo", "Say", |_, b| Ok(format!("{{\"got\":{b}}}"))),
                )
                .finish(),
        );
        let srv = TestServer::start(route(app)).await;
        let p = |h: &'static [(&'static str, &'static str)], body: &str| {
            srv.request(Method::POST, "/demo.v1.Echo/Say", h, Some(body.as_bytes().to_vec()))
        };
        let ok = p(&[("x-key", "k"), ("content-type", "application/json")], "{\"a\":1}").await;
        assert_eq!((ok.status, ok.text().as_str()), (200, "{\"got\":{\"a\":1}}"));
        assert_eq!(ok.header("content-type"), Some("application/json; charset=utf-8"));
        let denied = p(&[], "{}").await;
        assert_eq!(denied.status, 401);
        assert!(denied.text().contains("unauthenticated"));
        let page = srv.request(Method::GET, "/", &[], None).await;
        assert_eq!(page.header("cache-control"), Some("public, max-age=30"));
    }

    #[tokio::test]
    async fn production_layers_keep_pages_and_actions_working() {
        use reaxt::backend::Module;
        let app = Arc::new(
            App::new("pl")
                .page("index.rs", RenderMode::Ssr, |_| VNode::element("h1").child(VNode::text("hi")).build())
                .module(Module::new().method("demo.v1.Echo", "Say", |_, b| Ok(format!("{{\"got\":{b}}}"))))
                .finish(),
        );
        let srv = TestServer::start(route_production(app)).await;
        let top = srv.request(Method::GET, "/", &[], None).await;
        assert_eq!(top.status, 200);
        assert!(top.text().contains("<h1>hi</h1>"));
        let a = srv.request(Method::POST, "/demo.v1.Echo/Say", &[("content-type", "application/json")], Some(b"{\"a\":1}".to_vec())).await;
        assert_eq!((a.status, a.text().as_str()), (200, "{\"got\":{\"a\":1}}"));
    }

    #[tokio::test]
    async fn streaming_page_arrives_as_shell_then_replacements_over_rpoem() {
        use reaxt::stream::{chunks, slot, Pending};
        let app = Arc::new(
            App::new("st")
                .stream(
                    "/live",
                    Arc::new(|| {
                        let shell = VNode::element("main").child(slot(0, VNode::text("wait"))).build();
                        chunks(&shell, vec![Pending::new(|| VNode::text("ready"))])
                    }),
                )
                .finish(),
        );
        let srv = TestServer::start(route(app)).await;
        let r = srv.request(Method::GET, "/live", &[], None).await;
        assert_eq!(r.status, 200);
        let t = r.text();
        assert!(t.starts_with("<!doctype html>") && t.ends_with("</body></html>"), "{t}");
        assert!(t.contains("id=\"rx-s-0\"") && t.contains("<template id=\"rx-r-0\">ready</template>"), "{t}");
        assert!(t.find("wait").unwrap() < t.find("<template").unwrap(), "シェルが先、差し替えが後");
    }

    /// 実TCPで時刻つきに読む: 各フレーム(チャンク)の到着時刻を記録する。
    async fn read_timed(addr: std::net::SocketAddr, path: &str) -> Vec<(std::time::Duration, String)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let t0 = std::time::Instant::now();
        let mut acc = String::new();
        let mut out: Vec<(std::time::Duration, String)> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            let piece = String::from_utf8_lossy(&buf[..n]).into_owned();
            acc.push_str(&piece);
            out.push((t0.elapsed(), piece));
        }
        let _ = acc;
        out
    }

    #[tokio::test]
    async fn async_stream_sends_shell_first_then_out_of_order_and_times_out() {
        use reaxt::stream::{slot, AsyncPending};
        use std::time::Duration;
        let app = Arc::new(
            App::new("as")
                .stream_async(
                    "/live",
                    Arc::new(|| {
                        let shell = VNode::element("main")
                            .child(slot(0, VNode::text("wait0")))
                            .child(slot(1, VNode::text("wait1")))
                            .child(slot(2, VNode::text("wait2")))
                            .build();
                        let p = vec![
                            // 0: 遅い(400ms)、1: 速い(50ms)、2: 期限(100ms)を超える(5秒かかる)
                            AsyncPending::new(async {
                                tokio::time::sleep(Duration::from_millis(400)).await;
                                VNode::text("slow0")
                            }),
                            AsyncPending::new(async {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                VNode::text("fast1")
                            }),
                            AsyncPending::new(async {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                VNode::text("never2")
                            })
                            .timeout(Duration::from_millis(100), VNode::text("timeout2")),
                        ];
                        (shell, p)
                    }),
                )
                .finish(),
        );
        let (addr, _h) = open_runo_poem_compat::Server::new(open_runo_poem_compat::TcpListener::bind(([127, 0, 0, 1], 0)))
            .run(route(app))
            .await
            .unwrap();
        let frames = read_timed(addr, "/live").await;
        let all: String = frames.iter().map(|(_, s)| s.as_str()).collect();
        // 1) シェルは何も待たずに先に届く(最初のフレームに fallback が入り、差し替えはまだ無い)
        let first = &frames[0];
        assert!(first.0 < Duration::from_millis(40), "シェルが遅れた: {:?}", first.0);
        assert!(first.1.contains("wait0") && !first.1.contains("<template"), "{}", first.1);
        // 2) 終わった順(順不同): fast1 → timeout2 → slow0
        let p = |needle: &str| all.find(needle).unwrap_or_else(|| panic!("{needle} が無い: {all}"));
        assert!(p("fast1") < p("timeout2") && p("timeout2") < p("slow0"), "{all}");
        // 3) 期限超過の境界は5秒待たず、全体は400ms前後で終わる(遅い1つが全体を止めない)
        let total = frames.last().unwrap().0;
        assert!(total < Duration::from_millis(1500), "全体が遅い: {total:?}");
        assert!(!all.contains("never2") && all.ends_with("0\r\n\r\n"), "{all}");
        // 4) 各差し替えは、解決した時刻に届く(まとめて最後に来ない)
        let t_fast = frames.iter().find(|(_, s)| s.contains("fast1")).unwrap().0;
        assert!(t_fast < Duration::from_millis(300), "fast1 が遅れて届いた: {t_fast:?}");
    }

    #[tokio::test]
    async fn gateway_and_manifest_over_rpoem() {
        use reaxt::app::Response;
        let app = Arc::new(
            App::new("gw")
                .page("index.rs", RenderMode::Ssr, |_| VNode::element("p").build())
                .gateway(
                    "/_reaxt/graphql",
                    Arc::new(|_req, _call| Response {
                        status: 200,
                        content_type: "application/json; charset=utf-8",
                        headers: vec![],
                        bytes: None, chunks: None, streaming: None,
                        body: "{\"ok\":true}".to_string(),
                    }),
                )
                .finish(),
        );
        let srv = TestServer::start(route(app)).await;
        let g = srv.request(Method::POST, "/_reaxt/graphql", &[], Some(b"{}".to_vec())).await;
        assert_eq!((g.status, g.text().as_str()), (200, "{\"ok\":true}"));
        let m = srv.request(Method::GET, "/_reaxt/manifest.json", &[], None).await;
        assert_eq!(m.status, 200);
        assert_eq!(m.text(), "{\"version\":1,\"actions\":[]}");
    }

    #[tokio::test]
    async fn serves_reaxt_pages_over_rpoem() {
        let app = Arc::new(
            App::new("rp")
                .page("index.rs", RenderMode::Ssr, |_| VNode::element("h1").child(VNode::text("top")).build())
                .page("users/[id].rs", RenderMode::SsrHydrate, |p| {
                    VNode::element("b").child(VNode::text(p[0].1.clone())).build()
                })
                .page("docs/[...p].rs", RenderMode::Ssr, |p| {
                    VNode::element("i").child(VNode::text(p[0].1.clone())).build()
                })
                .finish(),
        );
        let srv = TestServer::start(route(app)).await;
        let top = srv.request(Method::GET, "/", &[], None).await;
        assert_eq!(top.status, 200);
        assert!(top.text().contains("<h1>top</h1>"));
        assert_eq!(top.header("x-content-type-options"), Some("nosniff"));
        let u = srv.request(Method::GET, "/users/%3C7%3E", &[], None).await;
        assert!(u.text().contains("<b>%3C7%3E</b>"));
        assert!(u.text().contains("boot.js"));
        let d = srv.request(Method::GET, "/docs/a/b/c", &[], None).await;
        assert!(d.text().contains("<i>a/b/c</i>"));
        assert_eq!(srv.request(Method::GET, "/nope", &[], None).await.status, 404);
    }
}

#[cfg(test)]
mod ws_tests {
    use super::*;
    use reaxt::app::{Params, WsEvent, WsOut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// クライアント→サーバーの(マスク付き)テキストフレーム。
    fn frame(text: &str) -> Vec<u8> {
        let k = [1u8, 2, 3, 4];
        let mut f = vec![0x81, 0x80 | text.len() as u8];
        f.extend_from_slice(&k);
        f.extend(text.bytes().enumerate().map(|(i, b)| b ^ k[i % 4]));
        f
    }

    async fn read_text(s: &mut tokio::net::TcpStream) -> String {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).await.unwrap();
        assert_eq!(h[0], 0x81, "text frame");
        let mut b = vec![0u8; (h[1] & 0x7f) as usize];
        s.read_exact(&mut b).await.unwrap();
        String::from_utf8(b).unwrap()
    }

    #[tokio::test]
    async fn websocket_route_over_real_socket() {
        let app = Arc::new(
            App::new("ws")
                .ws(
                    "chat/[room].rs",
                    Arc::new(|p: &Params, ev: &WsEvent| {
                        let room = p[0].1.clone();
                        match ev {
                            WsEvent::Open => vec![WsOut::Send(format!("joined {room}"))],
                            WsEvent::Text(t) if t == "bye" => vec![WsOut::Send("bye".into()), WsOut::Close],
                            WsEvent::Text(t) => vec![WsOut::Send(format!("{room}: {t}"))],
                            WsEvent::Close => vec![],
                        }
                    }),
                )
                .finish(),
        );
        let (addr, _h) = open_runo_poem_compat::Server::new(open_runo_poem_compat::TcpListener::bind(([127, 0, 0, 1], 0)))
            .run(route(app))
            .await
            .unwrap();
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /chat/lobby HTTP/1.1\r\nhost: x\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n").await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(s.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(head.to_lowercase().contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="), "{head}");
        assert_eq!(read_text(&mut s).await, "joined lobby");
        s.write_all(&frame("hi")).await.unwrap();
        assert_eq!(read_text(&mut s).await, "lobby: hi");
        s.write_all(&frame("bye")).await.unwrap();
        assert_eq!(read_text(&mut s).await, "bye");
    }
}
