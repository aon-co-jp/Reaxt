//! 開発サーバー: 画面(Csr)+ GraphQLへの中継(Server Action)。
//! 使い方: serve [pkgディレクトリ] [ポート] ; 環境変数 ARUARU_GQL=host:port(既定 127.0.0.1:4100)
#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn http_post(addr: &str, path: &str, body: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::time::Duration;
    let mut s = std::net::TcpStream::connect(addr).map_err(|e| format!("接続できません {addr}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    // 管理操作・管理用GraphQLにはサーバー側の環境変数のトークンを付ける(ブラウザへは出さない)
    let tok = std::env::var("ARUARU_DB_ADMIN_TOKEN").map(|t| format!("x-admin-token: {t}\r\n")).unwrap_or_default();
    let req = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{tok}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").ok_or("不正な応答")?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!("上流が {}", head.lines().next().unwrap_or("")));
    }
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut r = rest;
        loop {
            let (len, tail) = r.split_once("\r\n").ok_or("chunk")?;
            let n = usize::from_str_radix(len.trim(), 16).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            out.push_str(tail.get(..n).ok_or("chunk長")?);
            r = tail.get(n + 2..).ok_or("chunk終端")?;
        }
        Ok(out)
    } else {
        Ok(rest.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use aruaru_admin_reaxt::{GqlOut, GQL};
    use reaxt::app::{App, RouteRule};
    use reaxt::backend::Denied;
    use reaxt::RenderMode;
    use reaxt_actions::ModuleActions;
    use std::sync::Arc;

    let mut a = std::env::args().skip(1);
    let pkg = a.next().unwrap_or_else(|| "examples/aruaru-admin/pkg".into());
    let port = a.next().unwrap_or_else(|| "8770".into());
    let upstream = std::env::var("ARUARU_GQL").unwrap_or_else(|_| "127.0.0.1:4100".into());
    let read = |f: &str| std::fs::read(format!("{pkg}/{f}")).unwrap_or_else(|e| panic!("{pkg}/{f}: {e}"));

    let up2 = upstream.clone();
    let up3 = upstream.clone();
    let app = App::new("aruaru-admin (Reaxt)")
        .head("<link rel=\"stylesheet\" href=\"/_reaxt/app.css\">")
        .global_guard(reaxt_actions::csrf_guard)
        .module(reaxt::backend::Module::new().action(&aruaru_admin_reaxt::SQL, move |_, i: aruaru_admin_reaxt::SqlIn| {
            // 書き込みはこの専用口だけ(トークンなどはサーバー側に置き、ブラウザへは出さない)
            let m = aruaru_admin_reaxt::exec_sql_mutation(&i.sql, i.auto_commit).map_err(|e| Denied::new(400, e))?;
            let q = rust_json::quote_json(&m);
            http_post(&up2, "/graphql", &format!("{{\"query\":{q}}}")).map(|body| GqlOut { body }).map_err(|e| Denied::new(502, e))
        }).action(&aruaru_admin_reaxt::ops::ADM, move |_, i: aruaru_admin_reaxt::ops::AdmIn| {
            // 管理操作は許可リストのメソッドだけ中継(トークンはサーバー側の環境変数)
            const OK: &[&str] = &["CreateBackup", "RestoreBackup", "CrawlRegistry", "TestSourceConnection", "RunMigration", "RegisterFederatedSource", "DropFederatedSource", "FederatedQuery", "ExplainDistributed", "ClusterNodeOp", "RebalanceCluster"];
            if !OK.contains(&i.method.as_str()) {
                return Err(Denied::new(403, "許可されていないメソッド"));
            }
            http_post(&up3, &format!("/open_runo.aruaru.admin.Admin/{}", i.method), &i.body).map(|body| GqlOut { body }).map_err(|e| Denied::new(502, e))
        }).action(&GQL, move |_, i: aruaru_admin_reaxt::GqlIn| {
            // 読み取り専用の中継: mutation は通さない(管理操作は別途Connectで)
            if i.query.trim_start().starts_with("mutation") {
                return Err(Denied::new(403, "mutation は中継しません"));
            }
            let q = rust_json::quote_json(&i.query);
            let body = format!("{{\"query\":{q}}}");
            http_post(&upstream, "/graphql", &body).map(|body| GqlOut { body }).map_err(|e| Denied::new(502, e))
        }))
        .page("index.rs", RenderMode::Csr, |_| reaxt::react::VNode::text(""))
        .asset("/_reaxt/boot.js", "text/javascript; charset=utf-8", b"import init from \"/_reaxt/pkg/aruaru_admin_reaxt.js\";\ninit();\n".to_vec())
        .asset("/_reaxt/app.css", "text/css; charset=utf-8", include_bytes!("../app.css").to_vec())
        .asset("/_reaxt/pkg/aruaru_admin_reaxt.js", "text/javascript; charset=utf-8", read("aruaru_admin_reaxt.js"))
        .asset("/_reaxt/pkg/aruaru_admin_reaxt_bg.wasm", "application/wasm", read("aruaru_admin_reaxt_bg.wasm"))
        .route_rule("/_reaxt/", RouteRule::default().cache(0))
        .finish();
    let l = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    println!("http://127.0.0.1:{port}/");
    reaxt::server::serve(Arc::new(app), l);
}
