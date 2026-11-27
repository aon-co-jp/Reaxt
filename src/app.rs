//! ルート表(Next風のファイル規約)+routeRules(Nuxt風)+バックエンド(Nest風)を1つにまとめる。
//! ページは`Fn(&Params)->VNode`(サーバー/WASM共通の純関数)。
use crate::backend::{Backend, Module};
use crate::react::VNode;
use crate::{matches, sort_routes, ssr, RenderMode, Route};

pub type Params = Vec<(String, String)>;
type Page = Box<dyn Fn(&Params) -> VNode + Send + Sync>;

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// バイナリ応答(wasm等)。Someなら`body`の代わりにこちらを送る。
    pub bytes: Option<Vec<u8>>,
    /// ストリーミング応答のチャンク列。Someならホストは順に送れる(結合すると`body`と同じ)。
    pub chunks: Option<Vec<String>>,
    /// 非同期ストリーミング(本物の遅延を待つ)。Someなら、ホストは `body`/`chunks` ではなくこちらを駆動する。
    pub streaming: Option<crate::stream::AsyncStream>,
}

/// Nuxtの`routeRules`相当: パス接頭辞ごとの上書き。`/`終わりでなければ完全一致または`/`区切りの接頭辞。
#[derive(Default, Clone)]
pub struct RouteRule {
    pub mode: Option<RenderMode>,
    pub headers: Vec<(String, String)>,
    /// 指定すると`cache-control: public, max-age=N`を付ける(0なら`no-store`)。
    pub cache_secs: Option<u32>,
}

impl RouteRule {
    pub fn mode(mut self, m: RenderMode) -> Self {
        self.mode = Some(m);
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn cache(mut self, secs: u32) -> Self {
        self.cache_secs = Some(secs);
        self
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a str,
}

#[derive(Default)]
pub struct App {
    pages: Vec<(Route, Page)>,
    rules: Vec<(String, RouteRule)>,
    backend: Backend,
    /// 静的アセット(`/_reaxt/boot.js`、wasm等): (パス, content-type, 中身)。
    assets: Vec<(String, &'static str, Vec<u8>)>,
    title: String,
    /// `<head>`に足す任意のHTML(CSSの`<link>`など。信頼できる固定文字列だけを渡すこと)。
    head: String,
    /// 外部連携の入口(`POST path`)。Connectメソッドを呼ぶ関数を受け取る(GraphQLサブグラフ等で使う)。
    gateways: Vec<(String, Gateway)>,
    /// ストリーミングSSRのページ(`GET path`)。チャンク列(`stream::chunks`の結果)を返す関数。
    streams: Vec<(String, StreamPage)>,
    async_streams: Vec<(String, AsyncStreamPage)>,
    /// WebSocketのルート(`pages/`規約のパス)。実際の接続はホストが持ち、メッセージごとに `ws_handle` を呼ぶ。
    sockets: Vec<(Route, WsHandler)>,
}

/// 外部連携の入口: (リクエスト, Connectメソッド呼び出し関数) → 応答。
/// ストリーミングページ: チャンク列を返す関数。
pub type StreamPage = std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>;
/// 非同期ストリーミングページ: (シェル, 遅れて解決する各境界)を返す関数。`slot(i, fallback)` をシェルに置き、`pendings[i]` と対応させる。
pub type AsyncStreamPage = std::sync::Arc<dyn Fn() -> (VNode, Vec<crate::stream::AsyncPending>) + Send + Sync>;

/// WebSocketのイベント(ホストが接続の開閉とメッセージごとに渡す)。
#[derive(Debug, Clone, PartialEq)]
pub enum WsEvent {
    Open,
    Text(String),
    Close,
}

/// ハンドラが返す指示(ホストが順に実行する)。
#[derive(Debug, Clone, PartialEq)]
pub enum WsOut {
    /// この接続へ文字列を送る。
    Send(String),
    /// この接続を閉じる。
    Close,
}

/// WebSocketのハンドラ: (パスのパラメータ, イベント) → 指示の列。通信を持たない純関数なので、どのホスト(axum・tokio-tungstenite・Wasm)でも同じに動く。
pub type WsHandler = std::sync::Arc<dyn Fn(&Params, &WsEvent) -> Vec<WsOut> + Send + Sync>;

pub type Gateway = std::sync::Arc<dyn Fn(&Request, &dyn Fn(&str, &str) -> (u16, String)) -> Response + Send + Sync>;

impl App {
    /// `POST path` を外部連携の入口にする(例: Cosmo向けGraphQLサブグラフ `/_reaxt/graphql`)。
    pub fn gateway(mut self, path: &str, g: Gateway) -> Self {
        self.gateways.push((path.to_string(), g));
        self
    }

    /// `<head>`に足すHTML(例 `<link rel="stylesheet" href="/_reaxt/app.css">`)。
    pub fn head(mut self, html: &str) -> Self {
        self.head.push_str(html);
        self
    }

    /// 静的アセットを登録する(GET/HEADで返す。長期キャッシュはroute_ruleで)。
    pub fn asset(mut self, path: &str, content_type: &'static str, bytes: Vec<u8>) -> Self {
        self.assets.push((path.to_string(), content_type, bytes));
        self
    }

    /// アセットのパス一覧(ホスト側のルート登録用)。
    pub fn asset_paths(&self) -> Vec<String> {
        self.assets.iter().map(|(p, _, _)| p.clone()).collect()
    }

    pub fn new(title: impl Into<String>) -> Self {
        App { title: title.into(), ..Default::default() }
    }

    /// `pages/`規約のファイルパス(例 `users/[id].rs`)でページを登録する。
    pub fn page(
        mut self,
        file: &str,
        mode: RenderMode,
        f: impl Fn(&Params) -> VNode + Send + Sync + 'static,
    ) -> Self {
        let r = crate::route_from_path(file, mode)
            .unwrap_or_else(|| panic!("ページ規約に合わないパス: {file}"));
        self.pages.push((r, Box::new(f)));
        self
    }

    /// routeRules(接頭辞が長いものが優先)。
    pub fn route_rule(mut self, prefix: &str, rule: RouteRule) -> Self {
        self.rules.push((prefix.to_string(), rule));
        self
    }

    /// Nestのモジュール(provider・ガード・Connectメソッド)を登録する。
    pub fn module(mut self, m: Module) -> Self {
        self.backend.register(m);
        self
    }

    pub fn global_guard(
        mut self,
        g: impl Fn(&crate::backend::Ctx) -> Result<(), crate::backend::Denied> + Send + Sync + 'static,
    ) -> Self {
        self.backend.global_guard(g);
        self
    }

    /// 静的ルートを動的ルートより先に照合する並びにする。登録の最後に1回呼ぶ。
    pub fn finish(mut self) -> Self {
        let mut order: Vec<Route> = self.pages.iter().map(|(r, _)| r.clone()).collect();
        sort_routes(&mut order);
        self.pages.sort_by_key(|(r, _)| {
            order.iter().position(|x| x.pattern == r.pattern).unwrap_or(usize::MAX)
        });
        self.rules.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        self
    }

    /// ページのパターン(`/users/:id`、`/docs/*p`)。ホスト側(RPoem等)のルート登録用。
    pub fn patterns(&self) -> Vec<String> {
        self.pages.iter().map(|(r, _)| r.pattern.clone()).collect()
    }

    /// Connectメソッドのパス(`/pkg.Service/Method`)。
    pub fn method_paths(&self) -> Vec<String> {
        self.backend.paths()
    }

    /// ストリーミングSSRのページを登録する(`GET path`)。`f` は `stream::chunks(..)` の結果(シェル+差し替え)を返す。
    /// 応答は文書の頭(`<html>`〜シェル)・各差し替え・末尾(`</body></html>`)に分けて返し、結合すると有効なHTMLになる。
    pub fn stream(mut self, path: &str, f: StreamPage) -> Self {
        self.streams.push((path.to_string(), f));
        self
    }

    /// 非同期ストリーミングページを登録する(`GET path`)。シェルは待たずに先に送り、各境界は終わった順に差し替える。
    /// 非同期のホスト(`reaxt-rpoem`)が必要。同期ホスト(`server`)は501を返す。
    pub fn stream_async(mut self, path: &str, f: AsyncStreamPage) -> Self {
        self.async_streams.push((path.to_string(), f));
        self
    }

    /// 非同期ストリーミングページのパス一覧(ホスト側のルート登録用)。
    pub fn async_stream_paths(&self) -> Vec<String> {
        self.async_streams.iter().map(|(p, _)| p.clone()).collect()
    }

    fn async_stream_response(&self, f: &AsyncStreamPage) -> Response {
        let (shell, pendings) = f();
        let mut t = String::new();
        ssr::escape(&self.title, &mut t);
        let head = &self.head;
        let head = format!("<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{t}</title>{head}</head><body data-reaxt-mode=\"Ssr\">{}", ssr::render(&shell));
        let tail = "</body></html>".to_string();
        // 同期ホスト向けの退避: 差し替え前(fallback入り)の文書。非同期ホストは `streaming` を使う。
        let body = format!("{head}{tail}");
        Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            headers: vec![("cache-control".into(), "no-store".into())],
            bytes: None,
            body,
            chunks: None,
            streaming: Some(crate::stream::AsyncStream { head, pendings, tail }),
        }
    }

    /// ストリーミングページのパス一覧(ホスト側のルート登録用)。
    pub fn stream_paths(&self) -> Vec<String> {
        self.streams.iter().map(|(p, _)| p.clone()).collect()
    }

    fn stream_response(&self, f: &StreamPage) -> Response {
        let cs = f();
        let mut t = String::new();
        ssr::escape(&self.title, &mut t);
        let head = &self.head;
        let mut parts: Vec<String> = Vec::with_capacity(cs.len() + 1);
        let mut it = cs.into_iter();
        let shell = it.next().unwrap_or_default();
        parts.push(format!("<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{t}</title>{head}</head><body data-reaxt-mode=\"Ssr\">{shell}"));
        parts.extend(it);
        parts.push("</body></html>".to_string());
        Response { status: 200, content_type: "text/html; charset=utf-8", headers: vec![("cache-control".into(), "no-store".into())], bytes: None, body: parts.concat(), chunks: Some(parts), streaming: None }
    }

    /// WebSocketのルートを登録する(パスは`pages/`規約: `chat/[room].rs`)。
    pub fn ws(mut self, file: &str, f: WsHandler) -> Self {
        let r = crate::route_from_path(file, RenderMode::Ssr).unwrap_or_else(|| panic!("ページ規約に合わないパス: {file}"));
        self.sockets.push((r, f));
        self
    }

    /// WebSocketのパターン一覧(ホスト側のルート登録用)。
    pub fn ws_patterns(&self) -> Vec<String> {
        self.sockets.iter().map(|(r, _)| r.pattern.clone()).collect()
    }

    /// WebSocketのイベントを処理する。パスに合うルートが無ければ `None`(ホストは404/拒否にする)。
    pub fn ws_handle(&self, path: &str, ev: &WsEvent) -> Option<Vec<WsOut>> {
        let bare = path.split(['?', '#']).next().unwrap_or("");
        self.sockets.iter().find_map(|(r, h)| matches(r, bare).map(|p| h(&p, ev)))
    }

    /// 外部連携の入口(`POST`)のパス一覧(ホスト側のルート登録用)。
    pub fn gateway_paths(&self) -> Vec<String> {
        self.gateways.iter().map(|(p, _)| p.clone()).collect()
    }

    fn rule_for(&self, path: &str) -> Option<&RouteRule> {
        self.rules.iter().find_map(|(p, r)| {
            let ok = path == p
                || (p.ends_with('/') && path.starts_with(p.as_str()))
                || path.strip_prefix(p.as_str()).map_or(false, |rest| rest.starts_with('/'));
            ok.then_some(r)
        })
    }

    /// GETでページを返す簡易入口(テスト・開発サーバー用)。
    pub fn handle(&self, path: &str) -> Response {
        self.dispatch(&Request { method: "GET", path, headers: &[], body: "" })
    }

    /// 全入口: Connectメソッド(POST)→ページ(GET/HEAD)→404。
    pub fn dispatch(&self, req: &Request) -> Response {
        let bare = req.path.split(['?', '#']).next().unwrap_or("");
        if req.method == "GET" && bare == "/_reaxt/manifest.json" {
            return Response { status: 200, content_type: "application/json; charset=utf-8", headers: vec![("cache-control".into(), "no-store".into())], bytes: None, chunks: None, streaming: None, body: self.manifest() };
        }
        if req.method == "GET" {
            if let Some((_, f)) = self.async_streams.iter().find(|(p, _)| p == bare) {
                return self.async_stream_response(f);
            }
            if let Some((_, f)) = self.streams.iter().find(|(p, _)| p == bare) {
                return self.stream_response(f);
            }
        }
        if req.method == "POST" {
            if let Some((_, g)) = self.gateways.iter().find(|(p, _)| p == bare) {
                let call = |path: &str, body: &str| self.backend.call("POST", path, req.headers, body).unwrap_or((404, "{\"code\":\"not_found\",\"message\":\"unknown method\"}".to_string()));
                return g(req, &call);
            }
        }
        if let Some((status, json)) = self.backend.call(req.method, req.path, req.headers, req.body) {
            return Response {
                status,
                content_type: "application/json; charset=utf-8",
                headers: vec![("cache-control".into(), "no-store".into())],
                bytes: None, chunks: None, streaming: None,
                body: json,
            };
        }
        let path = req.path.split(['?', '#']).next().unwrap_or("/");
        let rule = self.rule_for(path);
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(r) = rule {
            headers.extend(r.headers.clone());
            if let Some(s) = r.cache_secs {
                headers.push((
                    "cache-control".into(),
                    if s == 0 { "no-store".into() } else { format!("public, max-age={s}") },
                ));
            }
        }
        if req.method != "GET" && req.method != "HEAD" {
            return Response { status: 405, content_type: "text/plain; charset=utf-8", headers, bytes: None, chunks: None, streaming: None, body: "method not allowed".into() };
        }
        if let Some((_, ct, bytes)) = self.assets.iter().find(|(p, _, _)| p == path) {
            return Response { status: 200, content_type: ct, headers, bytes: Some(bytes.clone()), chunks: None, streaming: None, body: String::new() };
        }
        for (route, page) in &self.pages {
            if let Some(params) = matches(route, path) {
                let mode = rule.and_then(|r| r.mode).unwrap_or(route.mode);
                crate::hydrate::reset_islands();
                let tree = page(&params);
                return Response { status: 200, content_type: "text/html; charset=utf-8", headers, bytes: None, chunks: None, streaming: None, body: self.document(mode, &tree) };
            }
        }
        let nf = VNode::element("h1").child(VNode::text("404 Not Found")).build();
        Response { status: 404, content_type: "text/html; charset=utf-8", headers, bytes: None, chunks: None, streaming: None, body: self.document(RenderMode::Ssr, &nf) }
    }

    /// 公開しているConnectメソッドの一覧(JS/TS・Cosmo連携が読む)。
    pub fn manifest(&self) -> String {
        let items: Vec<String> = self
            .backend
            .paths()
            .iter()
            .filter_map(|p| {
                let (s, m) = p.strip_prefix('/')?.rsplit_once('/')?;
                Some(format!("{{\"service\":{},\"method\":{},\"path\":{}}}", jstr(s), jstr(m), jstr(p)))
            })
            .collect();
        format!("{{\"version\":1,\"actions\":[{}]}}", items.join(","))
    }

    fn document(&self, mode: RenderMode, tree: &VNode) -> String {
        let mut t = String::new();
        ssr::escape(&self.title, &mut t);
        // Ssr=JS/WASM無し。それ以外はハイドレーション用の読み込み口(実体は段階2で生成)
        let script = match mode {
            RenderMode::Ssr => "",
            _ => "<script type=\"module\" src=\"/_reaxt/boot.js\"></script>",
        };
        let body = match mode {
            RenderMode::Csr => "<div id=\"reaxt-root\"></div>".to_string(),
            _ => format!("<div id=\"reaxt-root\">{}</div>", ssr::render(tree)),
        };
        let head = &self.head;
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{t}</title>{head}</head><body data-reaxt-mode=\"{mode:?}\">{body}{script}</body></html>"
        )
    }
}

fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Denied;

    fn app() -> App {
        App::new("T<")
            .page("[slug].rs", RenderMode::Ssr, |_| VNode::element("i").build())
            .page("users/[id].rs", RenderMode::SsrHydrate, |p| {
                VNode::element("b").child(VNode::text(p[0].1.clone())).build()
            })
            .page("about.rs", RenderMode::Csr, |_| VNode::element("i").build())
            .page("admin/index.rs", RenderMode::Ssr, |_| VNode::element("i").build())
            .route_rule("/admin", RouteRule::default().header("x-robots-tag", "noindex").cache(0))
            .route_rule("/users", RouteRule::default().mode(RenderMode::Ssr).cache(60))
            .module(Module::new().method("demo.v1.Hello", "Say", |_, b| {
                if b.is_empty() { Err(Denied::new(400, "empty")) } else { Ok(format!("{{\"echo\":{b}}}")) }
            }))
            .finish()
    }

    #[test]
    fn dispatch_params_404_and_modes() {
        let app = app();
        let r = app.handle("/users/<7>?x=1");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("<b>&lt;7&gt;</b>"));
        assert!(r.body.contains("<title>T&lt;</title>"));
        assert!(app.handle("/about").body.contains("<div id=\"reaxt-root\"></div>"));
        assert!(!app.handle("/x").body.contains("boot.js"));
        assert_eq!(app.handle("/a/b/c").status, 404);
    }

    #[test]
    fn route_rules_override_mode_headers_cache() {
        let app = app();
        let u = app.handle("/users/1");
        assert!(!u.body.contains("boot.js"), "routeRulesでSsrに上書きされ、JS無し");
        assert!(u.headers.contains(&("cache-control".into(), "public, max-age=60".into())));
        let a = app.handle("/admin");
        assert!(a.headers.contains(&("x-robots-tag".into(), "noindex".into())));
        assert!(a.headers.contains(&("cache-control".into(), "no-store".into())));
        assert!(app.handle("/administrator").headers.is_empty(), "接頭辞は/区切りのみ");
    }

    #[test]
    fn connect_method_and_405_for_pages() {
        let app = app();
        let post = |p: &str, b: &str| app.dispatch(&Request { method: "POST", path: p, headers: &[], body: b });
        let ok = post("/demo.v1.Hello/Say", "\"hi\"");
        assert_eq!((ok.status, ok.body.as_str()), (200, "{\"echo\":\"hi\"}"));
        assert!(ok.content_type.starts_with("application/json"));
        assert_eq!(post("/demo.v1.Hello/Say", "").status, 400);
        assert_eq!(post("/about", "x").status, 405);
        assert_eq!(app.method_paths(), vec!["/demo.v1.Hello/Say"]);
    }

    #[test]
    fn websocket_route_dispatches_events() {
        let app = App::new("t").ws(
            "chat/[room].rs",
            std::sync::Arc::new(|p: &Params, ev: &WsEvent| {
                let room = p.iter().find(|(k, _)| k == "room").map(|(_, v)| v.clone()).unwrap_or_default();
                match ev {
                    WsEvent::Open => vec![WsOut::Send(format!("joined {room}"))],
                    WsEvent::Text(t) if t == "bye" => vec![WsOut::Send("bye".into()), WsOut::Close],
                    WsEvent::Text(t) => vec![WsOut::Send(format!("{room}: {t}"))],
                    WsEvent::Close => vec![],
                }
            }),
        );
        assert_eq!(app.ws_patterns().len(), 1);
        assert_eq!(app.ws_handle("/chat/lobby", &WsEvent::Open), Some(vec![WsOut::Send("joined lobby".into())]));
        assert_eq!(app.ws_handle("/chat/lobby?x=1", &WsEvent::Text("hi".into())), Some(vec![WsOut::Send("lobby: hi".into())]));
        assert_eq!(app.ws_handle("/chat/a", &WsEvent::Text("bye".into())), Some(vec![WsOut::Send("bye".into()), WsOut::Close]));
        assert_eq!(app.ws_handle("/nope", &WsEvent::Open), None);
    }
}
