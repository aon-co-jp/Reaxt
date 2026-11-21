//! WunderGraph Cosmo(GraphQL連合)向けのサブグラフ入口。
//! Reaxtが公開するConnectメソッドを、GraphQLのMutationフィールド(`pkg_Service_Method(input: JSON): JSON`)として見せる。
//! Cosmo Routerはこのサブグラフを他のサブグラフと合成し、`POST /_reaxt/graphql`へ問い合わせる。
//! 対応する問い合わせの形: ルートのフィールドを並べ、引数は `input: $変数` か `input: リテラル`(オブジェクト・配列・文字列・数値・真偽・null・列挙・入れ子の`$変数`)。
//! 戻り値はJSONスカラーなので下位の選択は不要。認証・`x-reaxt`ヘッダーはRouter側のヘッダー伝播で付ける。
use crate::GqlType;
use reaxt::app::{App, Gateway, Request, Response};
use rust_json::{parse_light_secure, quote_json, LightLimits, LightValue};
use std::sync::{Arc, Mutex};

/// 型付きで公開するアクションの記録(`ModuleActions::action` / `entity` が登録する)。
/// 同じプロセスの全 `App` で共有する(キーはConnectのパス)。
#[derive(Default)]
struct Registry {
    /// パス → 出力の型
    outputs: Vec<(String, GqlType)>,
    /// エンティティ: (型名, 鍵フィールド, 解決するアクションのパス)
    entities: Vec<(String, Vec<String>, String)>,
}
static REGISTRY: Mutex<Registry> = Mutex::new(Registry { outputs: Vec::new(), entities: Vec::new() });

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// アクションの出力型を記録する(同じパスは上書き)。
pub fn register_output(path: &str, out: GqlType) {
    let mut r = registry();
    r.outputs.retain(|(p, _)| p != path);
    r.outputs.push((path.to_string(), out));
}

/// エンティティを記録する。`key` は入力の `Object`、`out` は出力の `Object`(その名前がエンティティの型名)。
pub fn register_entity(path: &str, key: GqlType, out: GqlType) {
    let (GqlType::Object { fields: kf, .. }, GqlType::Object { name, .. }) = (&key, &out) else {
        return; // 構造体でなければエンティティにできない(何も登録しない)
    };
    let keys: Vec<String> = kf.iter().map(|(n, _)| n.to_string()).collect();
    let mut r = registry();
    r.entities.retain(|(t, _, _)| t != name);
    r.entities.push((name.to_string(), keys, path.to_string()));
}

fn output_of(path: &str) -> Option<GqlType> {
    registry().outputs.iter().find(|(p, _)| p == path).map(|(_, t)| t.clone())
}

/// GraphQLの型名(フィールドの戻り値として書く形)。
fn gql_ref(t: &GqlType) -> String {
    match t {
        GqlType::Json => "JSON".into(),
        GqlType::Int => "Int!".into(),
        GqlType::Float => "Float!".into(),
        GqlType::Boolean => "Boolean!".into(),
        GqlType::Str => "String!".into(),
        GqlType::List(i) => format!("[{}]!", gql_ref(i)),
        GqlType::Object { name, .. } => format!("{name}!"),
        GqlType::Nullable(i) => gql_ref(i).trim_end_matches('!').to_string(),
    }
}

/// 型の中に出てくるオブジェクト型を集める(名前の重複は最初のものだけ)。
fn collect_objects<'a>(t: &'a GqlType, out: &mut Vec<(&'static str, &'a Vec<(&'static str, GqlType)>)>) {
    match t {
        GqlType::List(i) | GqlType::Nullable(i) => collect_objects(i, out),
        GqlType::Object { name, fields } => {
            if !out.iter().any(|(n, _)| n == name) {
                out.push((name, fields));
                for (_, ft) in fields {
                    collect_objects(ft, out);
                }
            }
        }
        _ => {}
    }
}

pub const PATH: &str = "/_reaxt/graphql";

/// `pkg.v1.Service` + `Method` → GraphQLのフィールド名。
pub fn field_name(service: &str, method: &str) -> String {
    let mut s: String = service.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    s.push('_');
    s.push_str(method);
    s
}

/// 読み取り用のメソッドか(名前が `Get` / `List` / `Read` で始まる)。GraphQLでは Query、それ以外は Mutation として見せる。
pub fn is_read_method(method: &str) -> bool {
    ["Get", "List", "Read"].iter().any(|p| method.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with(|c: char| c.is_ascii_uppercase())))
}

/// `(サービス, メソッド)` に分けたパス一覧(読み取り用, 変更用)。
fn split_paths(manifest_paths: &[String]) -> (Vec<(&str, &str)>, Vec<(&str, &str)>) {
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    for p in manifest_paths {
        if let Some((s, m)) = p.strip_prefix('/').and_then(|x| x.rsplit_once('/')) {
            if is_read_method(m) {
                reads.push((s, m));
            } else {
                writes.push((s, m));
            }
        }
    }
    (reads, writes)
}

/// 連合(Federation v2)対応のSDL。`Get*`/`List*`/`Read*` は Query、それ以外は Mutation。
pub fn sdl(manifest_paths: &[String]) -> String {
    let (reads, writes) = split_paths(manifest_paths);
    let ret = |s: &str, m: &str| output_of(&format!("/{s}/{m}")).map_or_else(|| "JSON".to_string(), |t| gql_ref(&t));
    let mut o = String::from(
        "extend schema @link(url: \"https://specs.apollo.dev/federation/v2.0\", import: [\"@shareable\", \"@key\"])\n\nscalar JSON\n\ntype Query {\n  reaxt_ready: Boolean! @shareable\n",
    );
    for (s, m) in &reads {
        o.push_str(&format!("  {}(input: JSON): {}\n", field_name(s, m), ret(s, m)));
    }
    o.push_str("}\n");
    if !writes.is_empty() {
        o.push_str("\ntype Mutation {\n");
        for (s, m) in &writes {
            o.push_str(&format!("  {}(input: JSON): {}\n", field_name(s, m), ret(s, m)));
        }
        o.push_str("}\n");
    }
    // 型付きの出力に出てくるオブジェクト型(エンティティには `@key`)
    let mut objs: Vec<(&'static str, &Vec<(&'static str, GqlType)>)> = Vec::new();
    let outs: Vec<GqlType> = manifest_paths.iter().filter_map(|p| output_of(p)).collect();
    for t in &outs {
        collect_objects(t, &mut objs);
    }
    let ents = registry().entities.clone();
    for (name, fields) in objs {
        let key = ents.iter().find(|(t, _, _)| t == name).map(|(_, k, _)| format!(" @key(fields: \"{}\")", k.join(" "))).unwrap_or_default();
        o.push_str(&format!("\ntype {name}{key} {{\n"));
        for (f, ft) in fields {
            o.push_str(&format!("  {f}: {}\n", gql_ref(ft)));
        }
        o.push_str("}\n");
    }
    o
}

struct Root {
    alias: String,
    field: String,
    /// 引数の全文(`input: ...` や `representations: ...`)。
    args: String,
    /// 下位の選択(`{ ... }` の中身)。無ければ None。
    sel: Option<String>,
}

/// 問い合わせ文字列から、ルートのフィールド(別名・名前・`input: $v`)を取り出す。
/// `operationName` で選ばれた操作だけを残す(他の操作は捨て、フラグメントは残す)。
/// 名前が見つからない/操作が1つだけなら、元の問い合わせをそのまま返す。
fn select_operation_text(q: &str, name: &str) -> String {
    let chars: Vec<char> = q.chars().collect();
    let (mut defs, mut depth, mut start, mut i) = (Vec::<String>::new(), 0i32, 0usize, 0usize);
    while i < chars.len() {
        match chars[i] {
            '#' => { let from = i; while i < chars.len() && chars[i] != '\n' { i += 1 } if chars[start..from].iter().all(|c| c.is_whitespace()) { start = i; } }
            '"' => { i += 1; while i < chars.len() && chars[i] != '"' { if chars[i] == '\\' { i += 1 } i += 1 } }
            '{' => depth += 1,
            '}' => { depth -= 1; if depth == 0 { defs.push(chars[start..=i].iter().collect()); start = i + 1; } }
            _ => {}
        }
        i += 1;
    }
    let is_op = |d: &str| { let t = d.trim_start(); t.starts_with("query") || t.starts_with("mutation") || t.starts_with("subscription") };
    let ops: Vec<&String> = defs.iter().filter(|d| is_op(d)).collect();
    if ops.len() < 2 { return q.to_string(); }
    let named = |d: &str| d.trim_start().split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).nth(1).map_or(false, |n| n == name);
    if !ops.iter().any(|d| named(d)) { return q.to_string(); }
    defs.iter().filter(|d| !is_op(d) || named(d)).cloned().collect::<Vec<_>>().join("\n")
}

fn parse_roots(q: &str) -> Result<Vec<Root>, String> {
    let b: Vec<char> = q.chars().collect();
    let mut i = 0;
    // コメントを除く
    let mut clean: Vec<char> = Vec::with_capacity(b.len());
    while i < b.len() {
        if b[i] == '#' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else {
            clean.push(b[i]);
            i += 1;
        }
    }
    let b = clean;
    // 最初の`{`(操作の本体)まで進む。変数定義の`(...)`は読み飛ばす
    let mut i = 0;
    let mut paren = 0;
    while i < b.len() {
        match b[i] {
            '(' => paren += 1,
            ')' => paren -= 1,
            '{' if paren == 0 => break,
            _ => {}
        }
        i += 1;
    }
    if i >= b.len() {
        return Err("問い合わせに { がありません".into());
    }
    i += 1;
    let mut roots = Vec::new();
    let ident = |i: &mut usize, b: &[char]| -> String {
        while *i < b.len() && (b[*i].is_whitespace() || b[*i] == ',') {
            *i += 1;
        }
        let st = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == '_') {
            *i += 1;
        }
        b[st..*i].iter().collect()
    };
    loop {
        while i < b.len() && (b[i].is_whitespace() || b[i] == ',') {
            i += 1;
        }
        if i >= b.len() {
            return Err("問い合わせが閉じていません".into());
        }
        if b[i] == '}' {
            break;
        }
        let first = ident(&mut i, &b);
        if first.is_empty() {
            return Err(format!("解釈できない文字 {:?}", b[i]));
        }
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        let (alias, field) = if i < b.len() && b[i] == ':' {
            i += 1;
            let f = ident(&mut i, &b);
            (first, f)
        } else {
            (first.clone(), first)
        };
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        let mut input_src = None;
        if i < b.len() && b[i] == '(' {
            let st = i + 1;
            let mut d = 0;
            while i < b.len() {
                match b[i] {
                    '(' => d += 1,
                    ')' => {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let args: String = b[st..i.min(b.len())].iter().collect();
            i += 1;
            input_src = Some(args.trim().to_string());
        }
        // 下位の選択(型付きの出力では結果の射影に使う。`JSON` を返すフィールドでは無視する)
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        let mut sel = None;
        if i < b.len() && b[i] == '{' {
            let st = i + 1;
            let mut d = 0;
            while i < b.len() {
                match b[i] {
                    '{' => d += 1,
                    '}' => {
                        d -= 1;
                        if d == 0 {
                            sel = Some(b[st..i].iter().collect::<String>());
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        roots.push(Root { alias, field, args: input_src.unwrap_or_default(), sel });
    }
    if roots.is_empty() {
        return Err("ルートのフィールドがありません".into());
    }
    Ok(roots)
}

/// GraphQLの値(リテラル・`$変数`)をJSONの文字列へ変換する。列挙値は文字列、オブジェクトのキーは引用符つきにする。
/// 変数が無い・未定義なら `null`。ブロック文字列(`"""`)は未対応。
fn gql_value_to_json(src: &str, vars: Option<&LightValue>) -> Result<String, String> {
    struct P<'a> {
        b: Vec<char>,
        i: usize,
        vars: Option<&'a LightValue>,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && (self.b[self.i].is_whitespace() || self.b[self.i] == ',') {
                self.i += 1;
            }
        }
        fn name(&mut self) -> String {
            let st = self.i;
            while self.i < self.b.len() && (self.b[self.i].is_ascii_alphanumeric() || matches!(self.b[self.i], '_' | '.' | '+' | '-')) {
                self.i += 1;
            }
            self.b[st..self.i].iter().collect()
        }
        fn value(&mut self, depth: usize) -> Result<String, String> {
            if depth > 32 {
                return Err("引数の入れ子が深すぎます".into());
            }
            self.ws();
            let Some(&c) = self.b.get(self.i) else { return Err("値がありません".into()) };
            match c {
                '$' => {
                    self.i += 1;
                    let n = self.name();
                    Ok(self.vars.and_then(|m| m.get(&n)).map(LightValue::to_json).unwrap_or_else(|| "null".into()))
                }
                '"' => {
                    if self.b[self.i..].starts_with(&['"', '"', '"']) {
                        return Err("ブロック文字列(\"\"\")は未対応です".into());
                    }
                    self.i += 1;
                    let mut o = String::from("\"");
                    while self.i < self.b.len() {
                        let ch = self.b[self.i];
                        self.i += 1;
                        match ch {
                            '\\' => {
                                o.push('\\');
                                if let Some(&n) = self.b.get(self.i) {
                                    o.push(n);
                                    self.i += 1;
                                }
                            }
                            '"' => {
                                o.push('"');
                                return Ok(o);
                            }
                            _ => o.push(ch),
                        }
                    }
                    Err("文字列が閉じていません".into())
                }
                '[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.ws();
                        match self.b.get(self.i) {
                            Some(']') => {
                                self.i += 1;
                                return Ok(format!("[{}]", items.join(",")));
                            }
                            None => return Err("配列が閉じていません".into()),
                            _ => items.push(self.value(depth + 1)?),
                        }
                    }
                }
                '{' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.ws();
                        match self.b.get(self.i) {
                            Some('}') => {
                                self.i += 1;
                                return Ok(format!("{{{}}}", items.join(",")));
                            }
                            None => return Err("オブジェクトが閉じていません".into()),
                            _ => {
                                let k = self.name();
                                if k.is_empty() {
                                    return Err("オブジェクトのキーが読めません".into());
                                }
                                self.ws();
                                if self.b.get(self.i) != Some(&':') {
                                    return Err("オブジェクトに : がありません".into());
                                }
                                self.i += 1;
                                let v = self.value(depth + 1)?;
                                items.push(format!("{}:{v}", quote_json(&k)));
                            }
                        }
                    }
                }
                _ => {
                    let t = self.name();
                    if t.is_empty() {
                        return Err(format!("解釈できない文字 {c:?}"));
                    }
                    if matches!(t.as_str(), "null" | "true" | "false") {
                        Ok(t)
                    } else if t.chars().next().is_some_and(|x| x.is_ascii_digit() || x == '-') {
                        t.parse::<f64>().map(|_| t.clone()).map_err(|_| format!("数値が不正です: {t}"))
                    } else {
                        Ok(quote_json(&t))
                    }
                }
            }
        }
    }
    let mut p = P { b: src.chars().collect(), i: 0, vars };
    let out = p.value(0)?;
    p.ws();
    if p.i < p.b.len() {
        return Err("引数の後ろに余分な文字があります".into());
    }
    Ok(out)
}

/// 選択(`{ a b: c d { e } ... on T { f } }`)の1項目。
#[derive(Debug)]
struct Sel {
    alias: String,
    name: String,
    children: Vec<Sel>,
    /// `... on T` のとき T
    on: Option<String>,
}

/// 選択の解釈に要るもの: 問い合わせ内の名前付きフラグメント(名前, 型, 本体)と変数。
struct Env<'a> {
    frags: Vec<(String, String, String)>,
    vars: Option<&'a LightValue>,
}

/// `fragment 名前 on 型 { ... }` を問い合わせ全文から取り出す(波括弧の対応で本体を切る)。
fn extract_fragments(q: &str) -> Vec<(String, String, String)> {
    let b: Vec<char> = q.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 < b.len() {
        let at_word = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_');
        if at_word && b[i..].starts_with(&['f', 'r', 'a', 'g', 'm', 'e', 'n', 't']) && b[i + 8].is_whitespace() {
            let mut j = i + 8;
            let word = |j: &mut usize| -> String {
                while *j < b.len() && b[*j].is_whitespace() {
                    *j += 1;
                }
                let st = *j;
                while *j < b.len() && (b[*j].is_ascii_alphanumeric() || b[*j] == '_') {
                    *j += 1;
                }
                b[st..*j].iter().collect()
            };
            let name = word(&mut j);
            let on = word(&mut j);
            let ty = word(&mut j);
            while j < b.len() && b[j] != '{' {
                j += 1;
            }
            if on == "on" && !name.is_empty() && j < b.len() {
                let st = j + 1;
                let mut d = 0;
                while j < b.len() {
                    match b[j] {
                        '{' => d += 1,
                        '}' => {
                            d -= 1;
                            if d == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j < b.len() {
                    out.push((name, ty, b[st..j].iter().collect()));
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// `@skip(if: ..)` / `@include(if: ..)` を読み、残すなら true。値は true / false / `$変数`。
fn parse_directives(b: &[char], i: &mut usize, env: &Env) -> Result<bool, String> {
    let mut keep = true;
    loop {
        while *i < b.len() && b[*i].is_whitespace() {
            *i += 1;
        }
        if b.get(*i) != Some(&'@') {
            return Ok(keep);
        }
        *i += 1;
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_alphanumeric() {
            *i += 1;
        }
        let name: String = b[st..*i].iter().collect();
        if name != "skip" && name != "include" {
            return Err(format!("ディレクティブ @{name} は未対応です(@skip / @include のみ)"));
        }
        while *i < b.len() && b[*i].is_whitespace() {
            *i += 1;
        }
        if b.get(*i) != Some(&'(') {
            return Err("ディレクティブに ( がありません".into());
        }
        let st = *i + 1;
        while *i < b.len() && b[*i] != ')' {
            *i += 1;
        }
        let arg: String = b[st..(*i).min(b.len())].iter().collect();
        *i += 1;
        let val = arg.trim().strip_prefix("if").map(str::trim_start).and_then(|x| x.strip_prefix(':')).map(str::trim).ok_or("if: 値 の形だけ対応です")?;
        let cond = match val {
            "true" => true,
            "false" => false,
            v => env.vars.and_then(|vs| vs.get(v.strip_prefix('$')?)).and_then(|x| x.as_bool()).ok_or("if: の変数が真偽値で渡されていません")?,
        };
        if (name == "skip" && cond) || (name == "include" && !cond) {
            keep = false;
        }
    }
}

fn parse_selection(src: &str, env: &Env) -> Result<Vec<Sel>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let v = parse_sel_list(&b, &mut i, 0, env)?;
    Ok(v)
}

fn parse_sel_list(b: &[char], i: &mut usize, depth: usize, env: &Env) -> Result<Vec<Sel>, String> {
    if depth > 16 {
        return Err("選択の入れ子が深すぎます".into());
    }
    let ident = |i: &mut usize| -> String {
        let st = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == '_') {
            *i += 1;
        }
        b[st..*i].iter().collect()
    };
    let ws = |i: &mut usize| {
        while *i < b.len() && (b[*i].is_whitespace() || b[*i] == ',') {
            *i += 1;
        }
    };
    let mut out = Vec::new();
    loop {
        ws(i);
        if *i >= b.len() || b[*i] == '}' {
            return Ok(out);
        }
        if b[*i] == '@' {
            return Err("選択内のディレクティブは未対応です".into());
        }
        if b[*i..].starts_with(&['.', '.', '.']) {
            *i += 3;
            ws(i);
            // `... on 型 { }` / `... @dir { }`(型なし) / `...名前`(名前付きフラグメント)
            let kw = if b.get(*i) == Some(&'@') || b.get(*i) == Some(&'{') { String::new() } else { ident(i) };
            if kw.is_empty() || kw == "on" {
                let ty = if kw == "on" {
                    ws(i);
                    ident(i)
                } else {
                    "*".to_string()
                };
                let keep = parse_directives(b, i, env)?;
                ws(i);
                if b.get(*i) != Some(&'{') {
                    return Err("`...` の後に { がありません".into());
                }
                *i += 1;
                let children = parse_sel_list(b, i, depth + 1, env)?;
                if b.get(*i) != Some(&'}') {
                    return Err("選択が閉じていません".into());
                }
                *i += 1;
                if keep {
                    out.push(Sel { alias: String::new(), name: "...".into(), children, on: Some(ty) });
                }
            } else {
                let keep = parse_directives(b, i, env)?;
                let (_, ty, body) = env.frags.iter().find(|(n, _, _)| *n == kw).cloned().ok_or_else(|| format!("フラグメント {kw} が見つかりません"))?;
                if keep {
                    let bb: Vec<char> = body.chars().collect();
                    let mut k = 0;
                    let children = parse_sel_list(&bb, &mut k, depth + 1, env)?;
                    out.push(Sel { alias: String::new(), name: "...".into(), children, on: Some(ty) });
                }
            }
            continue;
        }
        let first = ident(i);
        if first.is_empty() {
            return Err(format!("解釈できない文字 {:?}", b[*i]));
        }
        ws(i);
        let (alias, name) = if b.get(*i) == Some(&':') {
            *i += 1;
            ws(i);
            (first, ident(i))
        } else {
            (first.clone(), first)
        };
        ws(i);
        if b.get(*i) == Some(&'(') {
            // 引数は読み飛ばす(出力の射影には使わない)
            let mut d = 0;
            while *i < b.len() {
                match b[*i] {
                    '(' => d += 1,
                    ')' => {
                        d -= 1;
                        if d == 0 {
                            *i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
            ws(i);
        }
        let keep = parse_directives(b, i, env)?;
        ws(i);
        let mut children = Vec::new();
        if b.get(*i) == Some(&'{') {
            *i += 1;
            children = parse_sel_list(b, i, depth + 1, env)?;
            if b.get(*i) != Some(&'}') {
                return Err("選択が閉じていません".into());
            }
            *i += 1;
        }
        if keep {
            out.push(Sel { alias, name, children, on: None });
        }
    }
}

/// 型 `ty` の値 `v` を選択 `sel` で射影してJSONの文字列にする。
fn project(v: &LightValue, ty: &GqlType, sel: &[Sel]) -> String {
    match ty {
        GqlType::Nullable(inner) => {
            if matches!(v, LightValue::Null) {
                "null".into()
            } else {
                project(v, inner, sel)
            }
        }
        GqlType::List(inner) => match v.as_array() {
            Some(a) => format!("[{}]", a.iter().map(|x| project(x, inner, sel)).collect::<Vec<_>>().join(",")),
            None => "null".into(),
        },
        GqlType::Object { name, fields } => {
            let mut parts: Vec<String> = Vec::new();
            project_into(v, name, fields, sel, &mut parts);
            format!("{{{}}}", parts.join(","))
        }
        _ => v.to_json(),
    }
}

fn project_into(v: &LightValue, tname: &str, fields: &[(&'static str, GqlType)], sel: &[Sel], parts: &mut Vec<String>) {
    for s in sel {
        if let Some(on) = &s.on {
            if on == "*" || on == tname {
                project_into(v, tname, fields, &s.children, parts);
            }
            continue;
        }
        let val = if s.name == "__typename" {
            quote_json(tname)
        } else {
            match (fields.iter().find(|(n, _)| *n == s.name), v.get(&s.name)) {
                (Some((_, ft)), Some(x)) if !s.children.is_empty() => project(x, ft, &s.children),
                (_, Some(x)) => x.to_json(),
                _ => "null".into(),
            }
        };
        parts.push(format!("{}:{val}", quote_json(&s.alias)));
    }
}

/// 応答(`call` の本文)を、フィールドの出力型と選択に従って整える。型を持たない出力(`JSON`)はそのまま。
fn shape_output(path: &str, raw: &str, sel: Option<&str>, env: &Env) -> Result<String, String> {
    let (Some(ty), Some(sel)) = (output_of(path), sel) else { return Ok(raw.to_string()) };
    if ty == GqlType::Json {
        return Ok(raw.to_string());
    }
    let sels = parse_selection(sel, env)?;
    let v = parse_light_secure(raw, &LightLimits::small()).map_err(|e| e.to_string())?;
    Ok(project(&v, &ty, &sels))
}

/// `_entities(representations: [...])`: 表現ごとに、型に登録されたアクションで解決し、選択で射影する。
fn resolve_entities(r: &Root, vars: Option<&LightValue>, env: &Env, call: &dyn Fn(&str, &str) -> (u16, String)) -> Result<String, String> {
    let rest = r.args.strip_prefix("representations").map(str::trim_start).and_then(|x| x.strip_prefix(':')).ok_or("_entities には representations 引数が必要です")?;
    let reps_json = gql_value_to_json(rest.trim(), vars)?;
    let reps = parse_light_secure(&reps_json, &LightLimits::small()).map_err(|e| e.to_string())?;
    let reps = reps.as_array().ok_or("representations は配列です")?;
    let sels = parse_selection(r.sel.as_deref().unwrap_or(""), env)?;
    let ents = registry().entities.clone();
    let mut out = Vec::new();
    for rep in reps {
        let tname = rep.get("__typename").and_then(|t| t.as_str()).unwrap_or("");
        let Some((_, _, path)) = ents.iter().find(|(t, _, _)| t == tname) else {
            out.push("null".to_string());
            continue;
        };
        let (status, body) = call(path, &rep.to_json());
        if status != 200 {
            out.push("null".to_string());
            continue;
        }
        let ty = output_of(path).unwrap_or(GqlType::Json);
        let v = parse_light_secure(&body, &LightLimits::small()).map_err(|e| e.to_string())?;
        out.push(project(&v, &ty, &sels));
    }
    Ok(format!("[{}]", out.join(",")))
}

fn gql_error(msg: &str, code: &str, status: u16) -> String {
    format!("{{\"message\":{},\"extensions\":{{\"code\":{},\"status\":{status}}}}}", quote_json(msg), quote_json(code))
}

/// 1回のGraphQL要求(本文JSON)を処理して、GraphQL応答のJSONを返す。
pub fn execute(body: &str, paths: &[String], call: &dyn Fn(&str, &str) -> (u16, String)) -> (u16, String) {
    let v = match parse_light_secure(body, &LightLimits::small()) {
        Ok(v) => v,
        Err(e) => return (400, format!("{{\"errors\":[{}]}}", gql_error(&e.to_string(), "invalid_argument", 400))),
    };
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let vars = v.get("variables");
    // 連合のSDL問い合わせ
    if query.contains("_service") {
        return (200, format!("{{\"data\":{{\"_service\":{{\"sdl\":{}}}}}}}", quote_json(&sdl(paths))));
    }
    // open-cosmo のゲートウェイは、ルートのフィールド名をこの問い合わせで探す(`remote_subgraphs.rs`)。
    // 型や引数までは返さない最小の応答(`queryType`/`mutationType` の `fields { name }` だけ)。
    if query.contains("__schema") {
        let (reads, writes) = split_paths(paths);
        let list = |v: &[(&str, &str)]| v.iter().map(|(s, m)| format!("{{\"name\":{}}}", quote_json(&field_name(s, m)))).collect::<Vec<_>>().join(",");
        let mut q = String::from("{\"name\":\"reaxt_ready\"}");
        if !reads.is_empty() {
            q.push(',');
            q.push_str(&list(&reads));
        }
        return (
            200,
            format!(
                "{{\"data\":{{\"__schema\":{{\"queryType\":{{\"fields\":[{q}]}},\"mutationType\":{{\"fields\":[{}]}}}}}}}}",
                list(&writes)
            ),
        );
    }
    let selected = match v.get("operationName").and_then(|n| n.as_str()) {
        Some(n) if !n.is_empty() => select_operation_text(query, n),
        _ => query.to_string(),
    };
    let roots = match parse_roots(&selected) {
        Ok(r) => r,
        Err(e) => return (200, format!("{{\"data\":null,\"errors\":[{}]}}", gql_error(&e, "invalid_argument", 400))),
    };
    let env = Env { frags: extract_fragments(query), vars };
    let mut data = Vec::new();
    let mut errors = Vec::new();
    for r in roots {
        if r.field == "__typename" {
            data.push(format!("{}:\"Mutation\"", quote_json(&r.alias)));
            continue;
        }
        if r.field == "reaxt_ready" {
            data.push(format!("{}:true", quote_json(&r.alias)));
            continue;
        }
        if r.field == "_entities" {
            match resolve_entities(&r, vars, &env, call) {
                Ok(j) => data.push(format!("{}:{j}", quote_json(&r.alias))),
                Err(e) => {
                    errors.push(gql_error(&e, "invalid_argument", 400));
                    data.push(format!("{}:null", quote_json(&r.alias)));
                }
            }
            continue;
        }
        let target = paths.iter().find(|p| p.strip_prefix('/').and_then(|x| x.rsplit_once('/')).map_or(false, |(s, m)| field_name(s, m) == r.field));
        let Some(path) = target else {
            errors.push(gql_error(&format!("未知のフィールド {}", r.field), "not_found", 404));
            data.push(format!("{}:null", quote_json(&r.alias)));
            continue;
        };
        // 引数は `input: 値` だけ(無ければ入力なし)
        let input_src = if r.args.is_empty() {
            None
        } else {
            match r.args.strip_prefix("input").map(str::trim_start).and_then(|x| x.strip_prefix(':')).map(str::trim) {
                Some(v) if !v.is_empty() => Some(v.to_string()),
                _ => {
                    errors.push(gql_error("引数は input: 値 の形だけ対応です", "invalid_argument", 400));
                    data.push(format!("{}:null", quote_json(&r.alias)));
                    continue;
                }
            }
        };
        let input = match &input_src {
            Some(src) => match gql_value_to_json(src, vars) {
                Ok(j) => j,
                Err(e) => {
                    errors.push(gql_error(&e, "invalid_argument", 400));
                    data.push(format!("{}:null", quote_json(&r.alias)));
                    continue;
                }
            },
            None => "null".into(),
        };
        let (status, out) = call(path, &input);
        if status == 200 {
            match shape_output(path, &out, r.sel.as_deref(), &env) {
                Ok(shaped) => data.push(format!("{}:{}", quote_json(&r.alias), shaped)),
                Err(e) => {
                    errors.push(gql_error(&e, "invalid_argument", 400));
                    data.push(format!("{}:null", quote_json(&r.alias)));
                }
            }
        } else {
            let (code, msg) = match parse_light_secure(&out, &LightLimits::small()) {
                Ok(e) => (e.get("code").and_then(|x| x.as_str()).unwrap_or("unknown").to_string(), e.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string()),
                Err(_) => ("unknown".into(), out),
            };
            errors.push(gql_error(&msg, &code, status));
            data.push(format!("{}:null", quote_json(&r.alias)));
        }
    }
    let errs = if errors.is_empty() { String::new() } else { format!(",\"errors\":[{}]", errors.join(",")) };
    (200, format!("{{\"data\":{{{}}}{errs}}}", data.join(",")))
}

/// `App`に`POST /_reaxt/graphql`(Cosmo向けサブグラフ)を足す。メソッド一覧は呼び出し時点の登録分。
pub fn install(app: App) -> App {
    let paths = app.method_paths();
    let g: Gateway = Arc::new(move |req: &Request, call: &dyn Fn(&str, &str) -> (u16, String)| {
        let (status, body) = execute(req.body, &paths, call);
        Response { status, content_type: "application/json; charset=utf-8", headers: vec![("cache-control".into(), "no-store".into())], bytes: None, chunks: None, streaming: None, body }
    });
    app.gateway(PATH, g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<String> {
        vec!["/demo.v1.Counter/Add".into(), "/demo.v1.Counter/Fail".into()]
    }
    fn fake(path: &str, body: &str) -> (u16, String) {
        match path {
            "/demo.v1.Counter/Add" => (200, format!("{{\"echo\":{body}}}")),
            _ => (403, "{\"code\":\"permission_denied\",\"message\":\"no\"}".into()),
        }
    }

    #[test]
    fn sdl_lists_fields() {
        let s = sdl(&paths());
        assert!(s.contains("demo_v1_Counter_Add(input: JSON):"));
        assert!(s.contains("scalar JSON"));
    }

    #[test]
    fn operation_name_selects_one_operation_and_keeps_fragments() {
        let q = "query A { a } query B { b ...F } fragment F on T { x }";
        let s = select_operation_text(q, "B");
        assert!(s.contains("query B") && s.contains("fragment F") && !s.contains("query A"));
        assert_eq!(select_operation_text(q, "Z"), q); // 名前が無ければ元のまま
        assert_eq!(select_operation_text("{ a }", "B"), "{ a }");
    }

    #[test]
    fn operation_name_selection_ignores_braces_in_strings_and_comments() {
        let q = "query A { a(x: \"}\") } # } comment\nmutation B { b }";
        let s = select_operation_text(q, "B");
        assert!(s.contains("mutation B") && !s.contains("query A"), "{s}");
        let s = select_operation_text(q, "A");
        assert!(s.contains("query A") && !s.contains("mutation B"), "{s}");
    }

    #[test]
    fn executes_with_variables_alias_and_errors() {
        let q = "{\"query\":\"mutation M($a: JSON, $b: JSON) { x: demo_v1_Counter_Add(input: $a) demo_v1_Counter_Fail(input: $b) { y } }\",\"variables\":{\"a\":{\"by\":3},\"b\":null}}";
        let (st, out) = execute(q, &paths(), &fake);
        assert_eq!(st, 200);
        assert!(out.contains("\"x\":{\"echo\":{\"by\":3}}"), "{out}");
        assert!(out.contains("\"demo_v1_Counter_Fail\":null"), "{out}");
        assert!(out.contains("permission_denied"), "{out}");
    }

    #[test]
    fn answers_the_minimal_introspection_the_open_cosmo_gateway_uses() {
        let q = "{\"query\":\"{ __schema { queryType { fields { name } } mutationType { fields { name } } } }\"}";
        let (st, out) = execute(q, &paths(), &fake);
        assert_eq!(st, 200);
        assert!(out.contains("\"queryType\":{\"fields\":[{\"name\":\"reaxt_ready\"}]}"), "{out}");
        assert!(out.contains("\"mutationType\":{\"fields\":[{\"name\":\"demo_v1_Counter_Add\"}"), "{out}");
    }

    #[test]
    fn accepts_literal_arguments() {
        let q = "{\"query\":\"mutation($n: Int) { a: demo_v1_Counter_Add(input: {by: 1, tags: [\\\"x\\\", ok], deep: {n: $n, f: -2.5, t: true, z: null}}) }\",\"variables\":{\"n\":7}}";
        let (_, out) = execute(q, &paths(), &fake);
        assert!(out.contains("\"a\":{\"echo\":{\"by\":1,\"tags\":[\"x\",\"ok\"],\"deep\":{\"n\":7,\"f\":-2.5,\"t\":true,\"z\":null}}}"), "{out}");
        // 壊れたリテラルはエラーになり、他のフィールドには影響しない
        let (_, out) = execute("{\"query\":\"mutation { demo_v1_Counter_Add(input: {by: ) }\"}", &paths(), &fake);
        assert!(out.contains("invalid_argument"), "{out}");
    }

    #[test]
    fn rejects_unknown_fields_and_bad_json() {
        let (_, out) = execute("{\"query\":\"mutation { demo_v1_Counter_Add(other: 1) }\"}", &paths(), &fake);
        assert!(out.contains("input: 値"), "{out}");
        let (_, out) = execute("{\"query\":\"mutation { nope }\"}", &paths(), &fake);
        assert!(out.contains("not_found"), "{out}");
        let (st, _) = execute("{bad", &paths(), &fake);
        assert_eq!(st, 400);
    }

    #[test]
    fn read_methods_become_query_fields() {
        assert!(is_read_method("Get") && is_read_method("GetVisits") && is_read_method("ListAll") && is_read_method("ReadOne"));
        assert!(!is_read_method("Getter") && !is_read_method("Add") && !is_read_method("Settle"));
        let p = vec!["/demo.v1.Counter/Add".to_string(), "/demo.v1.Counter/GetTotal".to_string()];
        let s = sdl(&p);
        assert!(s.contains("type Query {\n  reaxt_ready: Boolean! @shareable\n  demo_v1_Counter_GetTotal(input: JSON):"), "{s}");
        assert!(s.contains("type Mutation {\n  demo_v1_Counter_Add(input: JSON):"), "{s}");
        // 変更用が1つも無ければ Mutation 型は出さない
        assert!(!sdl(&["/demo.v1.Counter/Get".to_string()]).contains("type Mutation"));
        // 読み取りは Query として実行でき、reaxt_ready と並べても動く
        let call = |path: &str, _b: &str| -> (u16, String) { (200, format!("\"{path}\"")) };
        let (_, out) = execute("{\"query\":\"{ reaxt_ready demo_v1_Counter_GetTotal }\"}", &p, &call);
        assert!(out.contains("\"reaxt_ready\":true") && out.contains("\"demo_v1_Counter_GetTotal\":\"/demo.v1.Counter/GetTotal\""), "{out}");
        let (_, out) = execute("{\"query\":\"{ __schema { queryType { fields { name } } } }\"}", &p, &call);
        assert!(out.contains("demo_v1_Counter_GetTotal") && out.contains("reaxt_ready"), "{out}");
    }

    #[test]
    fn answers_federation_sdl() {
        let (_, out) = execute("{\"query\":\"query { _service { sdl } }\"}", &paths(), &fake);
        assert!(out.contains("_service") && out.contains("scalar JSON"), "{out}");
    }

    #[derive(Debug)]
    struct EKey { id: String }
    crate::wire_struct!(EKey { id: String });
    #[derive(Debug)]
    struct Ent { id: String, n: u32 }
    crate::wire_struct!(Ent { id: String, n: u32 });

    #[test]
    fn named_fragments_and_directives_in_selection() {
        register_output("/t.v1.E/GetEnt", <Ent as crate::Wire>::gql_type());
        let p = vec!["/t.v1.E/GetEnt".to_string()];
        let call = |_: &str, _: &str| (200u16, "{\"id\":\"x\",\"n\":7}".to_string());
        let q = |x: bool| format!("{{\"query\":\"query($x:Boolean!){{ t_v1_E_GetEnt {{ id ...F @include(if:$x) }} }} fragment F on Ent {{ n }}\",\"variables\":{{\"x\":{x}}}}}");
        assert!(execute(&q(true), &p, &call).1.contains("{\"id\":\"x\",\"n\":7}"));
        let (_, o) = execute(&q(false), &p, &call);
        assert!(o.contains("{\"id\":\"x\"}") && !o.contains("\"n\""), "{o}");
        let q3 = "{\"query\":\"{ t_v1_E_GetEnt { id n @skip(if: true) ... @include(if: true) { n } } }\"}";
        assert!(execute(q3, &p, &call).1.contains("{\"id\":\"x\",\"n\":7}"));
    }

    #[test]
    fn option_is_nullable_and_round_trips() {
        use crate::Wire;
        assert_eq!(<Option<u32> as Wire>::gql_type(), GqlType::Nullable(Box::new(GqlType::Int)));
        assert_eq!(gql_ref(&<Option<String> as Wire>::gql_type()), "String");
        assert_eq!(Some(3u32).to_wire(), "3");
        assert_eq!(None::<u32>.to_wire(), "null");
        assert_eq!(crate::decode::<Option<u32>>("null").unwrap(), None);
        assert_eq!(crate::decode::<Option<u32>>("5").unwrap(), Some(5));
        let t = <Option<Ent> as Wire>::gql_type();
        let v = parse_light_secure("null", &LightLimits::small()).unwrap();
        assert_eq!(project(&v, &t, &[]), "null");
    }

    #[test]
    fn typed_entity_sdl_and_entities_call() {
        register_entity("/t.v1.E/GetEnt", <EKey as crate::Wire>::gql_type(), <Ent as crate::Wire>::gql_type());
        register_output("/t.v1.E/GetEnt", <Ent as crate::Wire>::gql_type());
        let p = vec!["/t.v1.E/GetEnt".to_string()];
        let s = sdl(&p);
        assert!(s.contains("t_v1_E_GetEnt(input: JSON): Ent!"), "{s}");
        assert!(s.contains("type Ent @key(fields: \"id\")"), "{s}");
        assert!(s.contains("n: Int!"), "{s}");
        let call = |_: &str, b: &str| (200u16, format!("{{\"id\":\"x\",\"n\":7,\"b\":{b}}}"));
        let q = "{\"query\":\"query($r:[_Any!]!){ _entities(representations:$r){ ... on Ent { n } } }\",\"variables\":{\"r\":[{\"__typename\":\"Ent\",\"id\":\"x\"},{\"__typename\":\"Nope\"}]}}";
        let (st, out) = execute(q, &p, &call);
        assert_eq!(st, 200);
        assert!(out.contains("\"_entities\":[{\"n\":7},null]"), "{out}");
        let q2 = "{\"query\":\"{ t_v1_E_GetEnt(input:{id:\\\"x\\\"}) { id __typename } }\"}";
        let (_, o2) = execute(q2, &p, &call);
        assert!(o2.contains("{\"id\":\"x\",\"__typename\":\"Ent\"}"), "{o2}");
    }
}
