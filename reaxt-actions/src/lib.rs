//! Server Actions: 1つの定義(`Action<I, O>`)をサーバーとブラウザで共有する。
//!
//! - サーバー: `Module::action(&ACTION, |ctx, input| ...)`(型付き。入力JSONは上限つきで読む)
//! - クライアント: `call(&client, &ACTION, &input, |result| ...)`(コールバック式。非同期の応答で状態を更新する)
//!
//! 通信はConnect単純呼び出し(`POST /pkg.Service/Method`、本文JSON)のみで、RESTは作らない。
pub mod subgraph;
use reaxt::backend::{Ctx, Denied, Module};
use rust_json::{parse_light_secure, LightLimits};
use std::marker::PhantomData;

#[cfg(target_arch = "wasm32")]
pub mod web;

/// 入力`I`・出力`O`の型つき呼び出し口の定義。`const`で作れて、両側で同じ値を使う。
pub struct Action<I, O> {
    pub service: &'static str,
    pub method: &'static str,
    _p: PhantomData<fn(I) -> O>,
}

impl<I, O> Action<I, O> {
    pub const fn new(service: &'static str, method: &'static str) -> Self {
        Action { service, method, _p: PhantomData }
    }
    pub fn path(&self) -> String {
        format!("/{}/{}", self.service, self.method)
    }
}

/// サーバー側の登録(`Module`の拡張)。
pub trait ModuleActions: Sized {
    fn action<I, O, F>(self, a: &Action<I, O>, f: F) -> Self
    where
        I: Wire + 'static,
        O: Wire + 'static,
        F: Fn(&Ctx, I) -> Result<O, Denied> + Send + Sync + 'static;

    /// `action` に加えて、出力 `O`(`wire_struct!` の構造体)を `@key` のエンティティとしてCosmoのサブグラフに公開する。
    /// 入力 `I`(`wire_struct!`)のフィールドが鍵。ゲートウェイの `_entities` 呼び出しで、この関数が1件ずつ解決する。
    fn entity<I, O, F>(self, a: &Action<I, O>, f: F) -> Self
    where
        I: Wire + 'static,
        O: Wire + 'static,
        F: Fn(&Ctx, I) -> Result<O, Denied> + Send + Sync + 'static;
}

impl ModuleActions for Module {
    fn action<I, O, F>(self, a: &Action<I, O>, f: F) -> Self
    where
        I: Wire + 'static,
        O: Wire + 'static,
        F: Fn(&Ctx, I) -> Result<O, Denied> + Send + Sync + 'static,
    {
        subgraph::register_output(&a.path(), O::gql_type());
        self.method(a.service, a.method, move |ctx, body| {
            let input = decode::<I>(body).map_err(|e| Denied::new(400, e))?;
            let out = f(ctx, input)?;
            Ok(out.to_wire())
        })
    }

    fn entity<I, O, F>(self, a: &Action<I, O>, f: F) -> Self
    where
        I: Wire + 'static,
        O: Wire + 'static,
        F: Fn(&Ctx, I) -> Result<O, Denied> + Send + Sync + 'static,
    {
        subgraph::register_entity(&a.path(), I::gql_type(), O::gql_type());
        self.action(a, f)
    }
}

/// CSRF対策のガード: ブラウザの`WebClient`が付ける`x-reaxt: 1`を必須にする。
/// カスタムヘッダは他オリジンのページからはCORSの事前確認なしに送れないため、
/// 勝手なサイトからのフォーム送信やfetchを弾ける。`App::global_guard(csrf_guard)`で全体に掛ける。
pub fn csrf_guard(ctx: &Ctx) -> Result<(), Denied> {
    if ctx.header("x-reaxt") == Some("1") {
        Ok(())
    } else {
        Err(Denied::new(403, "missing x-reaxt header"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// 通信できなかった(fetch失敗など)
    Transport(String),
    /// サーバーが返したエラー(Connectのcode/message)
    Server { status: u16, code: String, message: String },
    /// 応答のJSONが期待した型でなかった
    Decode(String),
}

pub type RawResult = Result<(u16, String), String>;

/// HTTPで`POST`する部品(ブラウザ=fetch、テスト・SSR=同一プロセス)。応答はコールバックで返す。
pub trait Client {
    fn post(&self, path: &str, body: String, cb: Box<dyn FnOnce(RawResult)>);
}

/// Actionの入出力型が実装する、serde不要のJSON変換(読みはRS-JSONの`light`、上限・重複キー拒否つき)。
/// `to_wire`はコンパクトなJSON文字列、`from_wire`は読み込み済みの値から作る。
pub trait Wire: Sized {
    fn to_wire(&self) -> String;
    fn from_wire(v: &LightValue) -> Result<Self, String>;
    /// この型のGraphQL上の形(Cosmoのサブグラフが、結果を `JSON` ではなく型付きで見せるのに使う)。
    /// 既定は `Json`(構造を持たない)。`wire_struct!` は `Object` を返す。
    fn gql_type() -> GqlType {
        GqlType::Json
    }
}

/// `Wire` 型のGraphQL上の形。
#[derive(Clone, Debug, PartialEq)]
pub enum GqlType {
    /// 構造を持たない(GraphQLの `JSON` スカラー)。
    Json,
    Int,
    /// 小数(`f64`)。
    Float,
    Boolean,
    Str,
    List(Box<GqlType>),
    /// 値が無い(null)こともある(`Option<T>`)。
    Nullable(Box<GqlType>),
    /// 名前付きフィールドの構造体(`wire_struct!`)。
    Object { name: &'static str, fields: Vec<(&'static str, GqlType)> },
}

impl Wire for () {
    fn to_wire(&self) -> String {
        "null".into()
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        if *v == LightValue::Null { Ok(()) } else { Err("null が必要です".into()) }
    }
}
macro_rules! wire_int {
    ($($t:ty),*) => {$(
        impl Wire for $t {
            fn to_wire(&self) -> String { self.to_string() }
            fn from_wire(v: &LightValue) -> Result<Self, String> {
                v.as_i64().and_then(|n| <$t>::try_from(n).ok()).ok_or_else(|| "整数が必要です(範囲外かも)".to_string())
            }
            fn gql_type() -> $crate::GqlType {
                $crate::GqlType::Int
            }
        }
    )*};
}
wire_int!(u8, u16, u32, u64, i8, i16, i32, i64, usize, isize);
impl Wire for f64 {
    fn to_wire(&self) -> String {
        // JSONは NaN / 無限大を表せないので null にする
        if self.is_finite() { self.to_string() } else { "null".to_string() }
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        v.as_f64().ok_or_else(|| "数値が必要です".into())
    }
    fn gql_type() -> GqlType {
        GqlType::Float
    }
}
/// `f32` は `f64` に広げて扱う(小数点以下の見た目が変わらないよう、f32のまま文字列にする)。
impl Wire for f32 {
    fn to_wire(&self) -> String {
        if self.is_finite() { self.to_string() } else { "null".to_string() }
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        v.as_f64().map(|x| x as f32).ok_or_else(|| "数値が必要です".into())
    }
    fn gql_type() -> GqlType {
        GqlType::Float
    }
}
/// 2〜12個組のタプルは(標準ライブラリやserdeと同じく12個まで)、長さ固定のJSON配列として扱う(GraphQLには `JSON` として見せる)。
macro_rules! wire_tuple {
    ($n:expr; $($t:ident : $i:tt),+) => {
        impl<$($t: Wire),+> Wire for ($($t,)+) {
            fn to_wire(&self) -> String {
                let parts: Vec<String> = vec![$(self.$i.to_wire()),+];
                format!("[{}]", parts.join(","))
            }
            fn from_wire(v: &LightValue) -> Result<Self, String> {
                let a = v.as_array().ok_or("配列が必要です")?;
                if a.len() != $n {
                    return Err(format!("要素が{}個の配列が必要です", $n));
                }
                Ok(($($t::from_wire(&a[$i])?,)+))
            }
        }
    };
}
wire_tuple!(2; A: 0, B: 1);
wire_tuple!(3; A: 0, B: 1, C: 2);
wire_tuple!(4; A: 0, B: 1, C: 2, D: 3);
wire_tuple!(5; A: 0, B: 1, C: 2, D: 3, E: 4);
wire_tuple!(6; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5);
wire_tuple!(7; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6);
wire_tuple!(8; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7);
wire_tuple!(9; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8);
wire_tuple!(10; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9);
wire_tuple!(11; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10);
wire_tuple!(12; A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11);
/// `Box<T>` は中身と同じ扱い(再帰する型を作るときに使う)。
impl<T: Wire> Wire for Box<T> {
    fn to_wire(&self) -> String {
        (**self).to_wire()
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        T::from_wire(v).map(Box::new)
    }
    fn gql_type() -> GqlType {
        T::gql_type()
    }
}
impl Wire for bool {
    fn to_wire(&self) -> String {
        self.to_string()
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        v.as_bool().ok_or_else(|| "真偽値が必要です".into())
    }
    fn gql_type() -> GqlType {
        GqlType::Boolean
    }
}
impl Wire for String {
    fn to_wire(&self) -> String {
        rust_json::quote_json(self)
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        v.as_str().map(str::to_string).ok_or_else(|| "文字列が必要です".into())
    }
    fn gql_type() -> GqlType {
        GqlType::Str
    }
}
impl<T: Wire> Wire for Vec<T> {
    fn to_wire(&self) -> String {
        format!("[{}]", self.iter().map(Wire::to_wire).collect::<Vec<_>>().join(","))
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        v.as_array().ok_or("配列が必要です")?.iter().map(T::from_wire).collect()
    }
    fn gql_type() -> GqlType {
        GqlType::List(Box::new(T::gql_type()))
    }
}

impl<T: Wire> Wire for Option<T> {
    fn to_wire(&self) -> String {
        self.as_ref().map_or_else(|| "null".to_string(), Wire::to_wire)
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        if matches!(v, LightValue::Null) {
            Ok(None)
        } else {
            T::from_wire(v).map(Some)
        }
    }
    fn gql_type() -> GqlType {
        GqlType::Nullable(Box::new(T::gql_type()))
    }
}

/// 文字列キーの連想配列。GraphQLに連想配列の型は無いので、`JSON`として公開する。キーは並べ替えて出す(結果が毎回同じになる)。
impl<T: Wire> Wire for std::collections::BTreeMap<String, T> {
    fn to_wire(&self) -> String {
        let parts: Vec<String> = self.iter().map(|(k, v)| format!("{}:{}", rust_json::quote_json(k), v.to_wire())).collect();
        format!("{{{}}}", parts.join(","))
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        match v {
            LightValue::Object(fields) => fields.iter().map(|(k, x)| T::from_wire(x).map(|t| (k.clone(), t)).map_err(|e| format!("{k}: {e}"))).collect(),
            _ => Err("オブジェクトが必要です".into()),
        }
    }
}

/// `HashMap<String, T>` も同じ扱い。出力はキーを並べ替えるので、順序は毎回同じになる。
impl<T: Wire> Wire for std::collections::HashMap<String, T> {
    fn to_wire(&self) -> String {
        let sorted: std::collections::BTreeMap<&String, &T> = self.iter().collect();
        let parts: Vec<String> = sorted.iter().map(|(k, v)| format!("{}:{}", rust_json::quote_json(k), v.to_wire())).collect();
        format!("{{{}}}", parts.join(","))
    }
    fn from_wire(v: &LightValue) -> Result<Self, String> {
        match v {
            LightValue::Object(fields) => fields.iter().map(|(k, x)| T::from_wire(x).map(|t| (k.clone(), t)).map_err(|e| format!("{k}: {e}"))).collect(),
            _ => Err("オブジェクトが必要です".into()),
        }
    }
}

/// 上限つきでJSON本文を読んで`T`にする。
pub fn decode<T: Wire>(json: &str) -> Result<T, String> {
    let v = parse_light_secure(json, &LightLimits::small()).map_err(|e| e.to_string())?;
    T::from_wire(&v)
}

/// 必須の文字列フィールドを取り出す(`from_wire`の実装用)。
pub fn str_field(v: &LightValue, k: &str) -> Result<String, String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| format!("文字列フィールド {k} がありません"))
}
/// 必須の真偽値フィールド。
pub fn bool_field(v: &LightValue, k: &str) -> Result<bool, String> {
    v.get(k).and_then(|x| x.as_bool()).ok_or_else(|| format!("真偽値フィールド {k} がありません"))
}
/// 必須の整数フィールド。
pub fn int_field(v: &LightValue, k: &str) -> Result<i64, String> {
    v.get(k).and_then(|x| x.as_i64()).ok_or_else(|| format!("整数フィールド {k} がありません"))
}

/// 1つの文字列フィールドだけのオブジェクト(`{"k":"v"}`)を書き出す補助。
pub fn obj_json(fields: &[(&str, String)]) -> String {
    format!("{{{}}}", fields.iter().map(|(k, v)| format!("{}:{v}", rust_json::quote_json(k))).collect::<Vec<_>>().join(","))
}
pub use rust_json::quote_json;

/// 必須フィールドを任意の`Wire`型で取り出す(`wire_struct!`が使う)。
pub fn field<T: Wire>(v: &LightValue, k: &str) -> Result<T, String> {
    match v.get(k) {
        Some(x) => T::from_wire(x).map_err(|e| format!("{k}: {e}")),
        // 無いフィールドは `null` として読む。`Option<T>` なら None、それ以外は従来どおりエラー。
        None => T::from_wire(&LightValue::Null).map_err(|_| format!("フィールド {k} がありません")),
    }
}

/// 名前付きフィールドの構造体に`Wire`を実装する(各フィールドも`Wire`であること)。
/// 例: `wire_struct!(Add { by: u32 });` → `{"by":3}` との相互変換。
#[macro_export]
macro_rules! wire_struct {
    ($t:ident { $($f:ident : $ty:ty),* $(,)? }) => {
        impl $crate::Wire for $t {
            fn to_wire(&self) -> String {
                $crate::obj_json(&[$((stringify!($f), $crate::Wire::to_wire(&self.$f))),*])
            }
            fn from_wire(v: &$crate::LightValue) -> Result<Self, String> {
                Ok($t { $($f: $crate::field::<$ty>(v, stringify!($f))?),* })
            }
            fn gql_type() -> $crate::GqlType {
                $crate::GqlType::Object {
                    name: stringify!($t),
                    fields: vec![$((stringify!($f), <$ty as $crate::Wire>::gql_type())),*],
                }
            }
        }
    };
}
pub use rust_json::LightValue;

/// 型付きで呼び出す。`cb`は応答(または失敗)で1回だけ呼ばれる。
pub fn call<I, O>(client: &dyn Client, a: &Action<I, O>, input: &I, cb: impl FnOnce(Result<O, ActionError>) + 'static)
where
    I: Wire,
    O: Wire + 'static,
{
    client.post(
        &a.path(),
        input.to_wire(),
        Box::new(move |raw| {
            cb(match raw {
                Err(e) => Err(ActionError::Transport(e)),
                Ok((200, json)) => decode::<O>(&json).map_err(ActionError::Decode),
                Ok((status, json)) => match parse_light_secure(&json, &LightLimits::small()) {
                    Ok(e) => match (str_field(&e, "code"), str_field(&e, "message")) {
                        (Ok(code), Ok(message)) => Err(ActionError::Server { status, code, message }),
                        _ => Err(ActionError::Server { status, code: "unknown".into(), message: json }),
                    },
                    Err(_) => Err(ActionError::Server { status, code: "unknown".into(), message: json }),
                },
            })
        }),
    );
}

/// 同一プロセスの`App`を直接呼ぶクライアント(テスト・SSR時のデータ取得用)。
pub struct LocalClient<'a> {
    pub app: &'a reaxt::app::App,
    pub headers: Vec<(String, String)>,
}

impl Client for LocalClient<'_> {
    fn post(&self, path: &str, body: String, cb: Box<dyn FnOnce(RawResult)>) {
        let r = self.app.dispatch(&reaxt::app::Request { method: "POST", path, headers: &self.headers, body: &body });
        cb(Ok((r.status, r.body)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reaxt::app::App;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[derive(Debug, PartialEq)]
    struct Add {
        by: u32,
    }
    #[derive(Debug, PartialEq)]
    struct Total {
        total: u32,
    }
    const ADD: Action<Add, Total> = Action::new("demo.v1.Counter", "Add");
    crate::wire_struct!(Add { by: u32 });
    crate::wire_struct!(Total { total: u32 });

    #[derive(Debug, PartialEq)]
    struct Mixed {
        name: String,
        on: bool,
        ids: Vec<u8>,
    }
    crate::wire_struct!(Mixed { name: String, on: bool, ids: Vec<u8> });

    #[test]
    fn richer_types_round_trip() {
        use std::collections::HashMap;
        let m: HashMap<String, Vec<Option<i64>>> = decode(r#"{"b":[1,null],"a":[]}"#).unwrap();
        assert_eq!(m["b"], vec![Some(1), None]);
        assert_eq!(m.to_wire(), r#"{"a":[],"b":[1,null]}"#);
        let o: Option<Vec<Option<String>>> = decode(r#"["x",null]"#).unwrap();
        assert_eq!(o, Some(vec![Some("x".into()), None]));
        assert_eq!(decode::<Option<Vec<i64>>>("null").unwrap(), None);
        assert!(decode::<HashMap<String, i64>>(r#"{"k":"no"}"#).is_err());
    }

    #[test]
    fn bool_empty_string_and_nested_vec_round_trip() {
        assert_eq!(decode::<bool>("true").unwrap().to_wire(), "true");
        assert_eq!(decode::<bool>("false").unwrap().to_wire(), "false");
        assert_eq!(decode::<String>("\"\"").unwrap().to_wire(), "\"\"");
        let v: Vec<Vec<i64>> = decode("[[1,2],[],[3]]").unwrap();
        assert_eq!(v, vec![vec![1, 2], vec![], vec![3]]);
        assert_eq!(v.to_wire(), "[[1,2],[],[3]]");
        assert!(decode::<Vec<Vec<i64>>>("[1]").is_err());
    }

    #[test]
    fn wrong_json_types_are_rejected_and_strings_are_escaped() {
        assert!(decode::<bool>(r#""true""#).is_err());
        assert!(decode::<bool>("1").is_err());
        assert!(decode::<String>("5").is_err());
        assert!(decode::<String>("null").is_err());
        assert!(decode::<Vec<i64>>(r#"{"a":1}"#).is_err());
        assert!(decode::<i64>("true").is_err());
        let s = "a\"b\\c\nd".to_string();
        let wire = s.to_wire();
        assert!(!wire.contains('\n'));
        assert_eq!(decode::<String>(&wire).unwrap(), s);
    }

    #[test]
    fn f32_and_box_round_trip() {
        assert_eq!(decode::<f32>("1.5").unwrap(), 1.5f32);
        assert_eq!(1.5f32.to_wire(), "1.5");
        assert_eq!(f32::NAN.to_wire(), "null");
        assert!(decode::<f32>(r#""x""#).is_err());
        let b: Box<Vec<i64>> = decode("[1,2]").unwrap();
        assert_eq!(*b, vec![1, 2]);
        assert_eq!(b.to_wire(), "[1,2]");
        assert_eq!(<Box<f64> as Wire>::gql_type(), GqlType::Float);
        assert_eq!(<f32 as Wire>::gql_type(), GqlType::Float);
    }

    #[test]
    fn infinity_is_null_and_negative_numbers_round_trip() {
        assert_eq!(f64::INFINITY.to_wire(), "null");
        assert_eq!(f64::NEG_INFINITY.to_wire(), "null");
        assert_eq!(f32::INFINITY.to_wire(), "null");
        assert_eq!(decode::<f64>("-2.5").unwrap(), -2.5);
        assert_eq!(decode::<i64>("-7").unwrap(), -7);
        assert_eq!((-7i64).to_wire(), "-7");
        let b: Box<Option<i64>> = decode("null").unwrap();
        assert_eq!(*b, None);
    }

    #[test]
    fn tuples_are_fixed_length_arrays() {
        let t: (String, i64) = decode(r#"["a",2]"#).unwrap();
        assert_eq!(t, ("a".to_string(), 2));
        assert_eq!(t.to_wire(), r#"["a",2]"#);
        let t3: (bool, Option<i64>, Vec<u8>) = decode("[true,null,[1]]").unwrap();
        assert_eq!(t3, (true, None, vec![1]));
        let t6: (i64, i64, i64, i64, i64, String) = decode(r#"[1,2,3,4,5,"z"]"#).unwrap();
        assert_eq!(t6.to_wire(), r#"[1,2,3,4,5,"z"]"#);
        let t4: (i64, String, bool, Option<i64>) = decode(r#"[1,"a",false,null]"#).unwrap();
        assert_eq!(t4.to_wire(), r#"[1,"a",false,null]"#);
        let t5: (i64, i64, i64, i64, i64) = decode("[1,2,3,4,5]").unwrap();
        assert_eq!(t5.4, 5);
        assert!(decode::<(i64, i64, i64, i64, i64)>("[1,2,3,4]").is_err());
        assert!(decode::<(i64, i64, i64, i64)>("[1,2,3]").is_err());
        assert!(decode::<(i64, i64)>("[1]").is_err());
        assert!(decode::<(i64, i64)>("[1,2,3]").is_err());
        assert!(decode::<(i64, i64)>(r#"[1,"x"]"#).is_err());
        assert_eq!(<(i64, i64) as Wire>::gql_type(), GqlType::Json);
    }

    #[test]
    fn tuples_up_to_twelve_round_trip() {
        type T7 = (i64, i64, i64, i64, i64, i64, String);
        let t7: T7 = decode(r#"[1,2,3,4,5,6,"g"]"#).unwrap();
        assert_eq!(t7.6, "g");
        assert_eq!(t7.to_wire(), r#"[1,2,3,4,5,6,"g"]"#);
        type T12 = (u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, Option<i64>, bool);
        let t12: T12 = decode("[1,2,3,4,5,6,7,8,9,10,null,true]").unwrap();
        assert_eq!((t12.9, t12.10, t12.11), (10, None, true));
        assert_eq!(t12.to_wire(), "[1,2,3,4,5,6,7,8,9,10,null,true]");
        assert!(decode::<T7>("[1,2,3,4,5,6]").is_err());
        assert!(decode::<T12>("[1,2,3,4,5,6,7,8,9,10,null,true,1]").is_err());
        assert_eq!(<T12 as Wire>::gql_type(), GqlType::Json);
    }

    #[test]
    fn tuples_of_eight_nine_and_eleven_round_trip() {
        let t8: (i64, i64, i64, i64, i64, i64, i64, bool) =
            decode("[1,2,3,4,5,6,7,true]").unwrap();
        assert_eq!(t8.to_wire(), "[1,2,3,4,5,6,7,true]");
        let t9: (i64, i64, i64, i64, i64, i64, i64, i64, String) =
            decode(r#"[1,2,3,4,5,6,7,8,"i"]"#).unwrap();
        assert_eq!(t9.8, "i");
        type T11 = (u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, Option<String>);
        let t11: T11 = decode("[1,2,3,4,5,6,7,8,9,10,null]").unwrap();
        assert_eq!(t11.10, None);
        assert_eq!(t11.to_wire(), "[1,2,3,4,5,6,7,8,9,10,null]");
        assert!(decode::<T11>("[1,2,3,4,5,6,7,8,9,10]").is_err());
    }

    #[test]
    fn ten_tuple_and_integer_boundaries() {
        type T10 = (u8, u8, u8, u8, u8, u8, u8, u8, u8, String);
        let t10: T10 = decode(r#"[1,2,3,4,5,6,7,8,9,"j"]"#).unwrap();
        assert_eq!(t10.9, "j");
        assert_eq!(t10.to_wire(), r#"[1,2,3,4,5,6,7,8,9,"j"]"#);
        assert!(decode::<T10>("[1,2,3,4,5,6,7,8,9]").is_err());
        // 型の限界ちょうどは通り、1つ超えると拒否される
        assert_eq!(decode::<u8>("255").unwrap(), 255);
        assert!(decode::<u8>("256").is_err());
        assert_eq!(decode::<i8>("-128").unwrap(), -128);
        assert!(decode::<i8>("-129").is_err());
        // i64の最大値は JSON 数値(f64)経由のため読み込めない(既知の限界)。2^53 までは正確
        assert_eq!(decode::<i64>("9007199254740992").unwrap(), 9_007_199_254_740_992);
        assert_eq!(i64::MIN.to_wire(), "-9223372036854775808");
        assert!(decode::<u32>("-1").is_err());
    }


    #[test]
    fn mid_size_integer_boundaries_and_fractions() {
        assert_eq!(decode::<u16>("65535").unwrap(), 65535);
        assert!(decode::<u16>("65536").is_err());
        assert_eq!(decode::<i16>("-32768").unwrap(), -32768);
        assert!(decode::<i16>("32768").is_err());
        assert_eq!(decode::<i32>("2147483647").unwrap(), i32::MAX);
        assert!(decode::<i32>("2147483648").is_err());
        assert!(decode::<usize>("-1").is_err());
        assert_eq!(decode::<isize>("-5").unwrap(), -5);
        // 小数は整数として読めない
        assert!(decode::<i32>("1.5").is_err());
        assert!(decode::<u8>("0.5").is_err());
    }
    #[test]
    fn gql_types_of_basic_and_container_types() {
        use std::collections::{BTreeMap, HashMap};
        assert_eq!(<String as Wire>::gql_type(), GqlType::Str);
        assert_eq!(<bool as Wire>::gql_type(), GqlType::Boolean);
        assert_eq!(<i64 as Wire>::gql_type(), GqlType::Int);
        assert_eq!(
            <Vec<Option<String>> as Wire>::gql_type(),
            GqlType::List(Box::new(GqlType::Nullable(Box::new(GqlType::Str))))
        );
        assert_eq!(
            <Option<Vec<i64>> as Wire>::gql_type(),
            GqlType::Nullable(Box::new(GqlType::List(Box::new(GqlType::Int))))
        );
        // 連想配列はGraphQLに型が無いので JSON のまま
        assert_eq!(<BTreeMap<String, i64> as Wire>::gql_type(), GqlType::Json);
        assert_eq!(<HashMap<String, i64> as Wire>::gql_type(), GqlType::Json);
    }

    #[test]
    fn missing_field_is_none_for_option_and_error_otherwise() {
        #[derive(Debug)]
        struct P {
            a: u32,
            b: Option<String>,
        }
        wire_struct!(P { a: u32, b: Option<String> });
        let p: P = decode("{\"a\":1}").unwrap();
        assert_eq!((p.a, p.b), (1, None));
        let p: P = decode("{\"a\":1,\"b\":\"x\"}").unwrap();
        assert_eq!(p.b.as_deref(), Some("x"));
        assert!(decode::<P>("{\"b\":\"x\"}").unwrap_err().contains("a"));
    }

    #[test]
    fn map_output_is_sorted_and_non_objects_are_rejected() {
        use std::collections::{BTreeMap, HashMap};
        let mut h: HashMap<String, i64> = HashMap::new();
        for (k, v) in [("z", 1), ("a", 2), ("m", 3)] {
            h.insert(k.to_string(), v);
        }
        assert_eq!(h.to_wire(), r#"{"a":2,"m":3,"z":1}"#);
        let b: BTreeMap<String, bool> = BTreeMap::new();
        assert_eq!(b.to_wire(), "{}");
        assert!(decode::<BTreeMap<String, i64>>("[1,2]").is_err());
        assert!(decode::<HashMap<String, i64>>("null").is_err());
        assert!(decode::<HashMap<String, i64>>("5").is_err());
    }

    #[test]
    fn f64_round_trips_and_non_finite_is_null() {
        assert_eq!(decode::<f64>("1.5").unwrap(), 1.5);
        assert_eq!(2.25f64.to_wire(), "2.25");
        assert_eq!(f64::NAN.to_wire(), "null");
        assert!(decode::<f64>("\"x\"").is_err());
        assert_eq!(<f64 as Wire>::gql_type(), GqlType::Float);
    }

    #[test]
    fn more_integer_types_round_trip_and_reject_out_of_range() {
        assert_eq!(decode::<u64>("42").unwrap(), 42);
        assert_eq!(decode::<i8>("-5").unwrap(), -5);
        assert!(decode::<i8>("300").is_err());
        assert!(decode::<u64>("-1").is_err());
        assert_eq!(7isize.to_wire(), "7");
    }

    fn gql_type_str(t: &GqlType) -> &'static str {
        match t {
            GqlType::Nullable(i) => gql_type_str(i),
            GqlType::List(_) => "List",
            _ => "other",
        }
    }

    #[test]
    fn nested_option_vec_and_map_round_trip() {
        use std::collections::BTreeMap;
        let v: Vec<Option<u32>> = decode("[1,null,3]").unwrap();
        assert_eq!(v, vec![Some(1), None, Some(3)]);
        assert_eq!(v.to_wire(), "[1,null,3]");
        let o: Option<Vec<String>> = decode("[\"a\"]").unwrap();
        assert_eq!(o, Some(vec!["a".to_string()]));
        assert_eq!(decode::<Option<Vec<String>>>("null").unwrap(), None);
        let m: BTreeMap<String, u32> = decode("{\"b\":2,\"a\":1}").unwrap();
        assert_eq!(m.to_wire(), "{\"a\":1,\"b\":2}");
        assert!(decode::<BTreeMap<String, u32>>("{\"a\":\"x\"}").unwrap_err().contains("a"));
        assert!(decode::<BTreeMap<String, u32>>("[1]").is_err());
        assert_eq!(<BTreeMap<String, u32> as Wire>::gql_type(), GqlType::Json);
        assert_eq!(gql_type_str(&<Option<Vec<Option<u32>>> as Wire>::gql_type()), "List");
    }

    #[test]
    fn empty_containers_unicode_and_oversized_tuple() {
        use std::collections::BTreeMap;
        assert_eq!(decode::<Vec<i64>>("[]").unwrap(), Vec::<i64>::new());
        assert_eq!(Vec::<i64>::new().to_wire(), "[]");
        assert_eq!(decode::<BTreeMap<String, u32>>("{}").unwrap().len(), 0);
        let s: String = decode("\"日本語 ✓\"").unwrap();
        assert_eq!(s, "日本語 ✓");
        assert_eq!(decode::<String>(&s.to_wire()).unwrap(), s);
        let thirteen = "[1,2,3,4,5,6,7,8,9,10,11,12,13]";
        assert!(decode::<(u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8)>(thirteen).is_err());
    }

    #[test]
    fn option_vec_and_string_list_edge_cases() {
        // null and [] stay distinct for Option<Vec<_>>.
        assert_eq!(decode::<Option<Vec<i64>>>("null").unwrap(), None);
        assert_eq!(decode::<Option<Vec<i64>>>("[]").unwrap(), Some(vec![]));
        // Strings with quotes, backslashes and newlines survive inside a list.
        let v = vec!["a\"b".to_string(), "c\\d".to_string(), "e\nf".to_string()];
        assert_eq!(decode::<Vec<String>>(&v.to_wire()).unwrap(), v);
        // One null element is not a String.
        assert!(decode::<Vec<String>>(r#"["x",null]"#).is_err());
        assert_eq!(decode::<Vec<Option<String>>>(r#"["x",null]"#).unwrap(), vec![Some("x".to_string()), None]);
    }

    #[test]
    fn wire_struct_round_trips_and_rejects() {
        let m = Mixed { name: "a\"b\n".into(), on: true, ids: vec![1, 2, 3] };
        let json = m.to_wire();
        assert_eq!(decode::<Mixed>(&json).unwrap(), m);
        assert!(decode::<Mixed>("{\"name\":\"x\",\"on\":true}").unwrap_err().contains("ids"));
        assert!(decode::<Add>("{\"by\":-1}").is_err());
        assert!(decode::<Add>("{\"by\":1,\"by\":2}").is_err());
    }

    #[derive(Debug, PartialEq)]
    struct Opt {
        id: u32,
        note: Option<String>,
    }
    crate::wire_struct!(Opt { id: u32, note: Option<String> });

    #[test]
    fn wire_struct_option_field_null_missing_and_unknown_keys() {
        let a: Opt = decode(r#"{"id":1,"note":null}"#).unwrap();
        let b: Opt = decode(r#"{"id":1}"#).unwrap();
        assert_eq!(a, b);
        let c: Opt = decode(r#"{"id":2,"note":"hi"}"#).unwrap();
        assert_eq!(c.note.as_deref(), Some("hi"));
        assert_eq!(decode::<Opt>(&c.to_wire()).unwrap(), c);
        assert_eq!(decode::<Opt>(&a.to_wire()).unwrap(), a);
        // The required field is still required.
        assert!(decode::<Opt>(r#"{"note":"x"}"#).unwrap_err().contains("id"));
    }

    #[test]
    fn wire_struct_gql_type_lists_fields_in_order() {
        match <Opt as Wire>::gql_type() {
            GqlType::Object { name, fields } => {
                assert_eq!(name, "Opt");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0], ("id", GqlType::Int));
                assert_eq!(fields[1], ("note", GqlType::Nullable(Box::new(GqlType::Str))));
            }
            other => panic!("expected Object, got {other:?}"),
        }
        match <Mixed as Wire>::gql_type() {
            GqlType::Object { name, fields } => {
                assert_eq!(name, "Mixed");
                let names: Vec<_> = fields.iter().map(|f| f.0).collect();
                assert_eq!(names, ["name", "on", "ids"]);
                assert_eq!(fields[1].1, GqlType::Boolean);
                assert_eq!(fields[2].1, GqlType::List(Box::new(GqlType::Int)));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[derive(Debug, PartialEq)]
    struct Inner {
        n: i64,
    }
    crate::wire_struct!(Inner { n: i64 });
    #[derive(Debug, PartialEq)]
    struct Outer {
        first: Inner,
        rest: Vec<Inner>,
        extra: Option<Inner>,
    }
    crate::wire_struct!(Outer { first: Inner, rest: Vec<Inner>, extra: Option<Inner> });

    #[test]
    fn nested_wire_structs_round_trip_and_report_the_field_path() {
        let o: Outer = decode(r#"{"first":{"n":1},"rest":[{"n":2},{"n":3}]}"#).unwrap();
        assert_eq!(o.first, Inner { n: 1 });
        assert_eq!(o.rest.len(), 2);
        assert_eq!(o.extra, None);
        assert_eq!(decode::<Outer>(&o.to_wire()).unwrap(), o);
        // An error inside a nested struct names the outer field.
        let e = decode::<Outer>(r#"{"first":{"n":"x"},"rest":[]}"#).unwrap_err();
        assert!(e.contains("first"), "{e}");
        // The GraphQL type nests too.
        match <Outer as Wire>::gql_type() {
            GqlType::Object { name, fields } => {
                assert_eq!(name, "Outer");
                assert!(matches!(&fields[0].1, GqlType::Object { name: "Inner", .. }));
                assert!(matches!(&fields[1].1, GqlType::List(_)));
                assert!(matches!(&fields[2].1, GqlType::Nullable(_)));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    fn app() -> App {
        let counter = Arc::new(AtomicU32::new(0));
        App::new("t")
            .module(
                Module::new()
                    .guard(|c| if c.header("x-key") == Some("k") { Ok(()) } else { Err(Denied::new(401, "no key")) })
                    .action(&ADD, move |_, i: Add| {
                        if i.by > 100 {
                            return Err(Denied::new(400, "too big"));
                        }
                        Ok(Total { total: counter.fetch_add(i.by, Ordering::SeqCst) + i.by })
                    }),
            )
            .finish()
    }

    fn run<O: Wire + 'static>(c: &LocalClient, a: &Action<Add, O>, by: u32) -> Result<O, ActionError> {
        let out = Rc::new(RefCell::new(None));
        let o2 = out.clone();
        call(c, a, &Add { by }, move |r| *o2.borrow_mut() = Some(r));
        let r = out.borrow_mut().take().expect("callback called once");
        r
    }

    #[test]
    fn typed_round_trip_errors_and_guard() {
        let app = app();
        let ok = LocalClient { app: &app, headers: vec![("x-key".into(), "k".into())] };
        assert_eq!(run(&ok, &ADD, 2), Ok(Total { total: 2 }));
        assert_eq!(run(&ok, &ADD, 3), Ok(Total { total: 5 }), "サーバー側の状態が保たれる");
        match run(&ok, &ADD, 101) {
            Err(ActionError::Server { status: 400, code, message }) => {
                assert_eq!(code, "invalid_argument");
                assert_eq!(message, "too big");
            }
            other => panic!("{other:?}"),
        }
        let anon = LocalClient { app: &app, headers: vec![] };
        assert!(matches!(run(&anon, &ADD, 1), Err(ActionError::Server { status: 401, .. })));
        // 型が合わない応答はDecodeエラー
        const WRONG: Action<Add, Vec<String>> = Action::new("demo.v1.Counter", "Add");
        assert!(matches!(run(&ok, &WRONG, 1), Err(ActionError::Decode(_))));
    }

    #[test]
    fn csrf_guard_requires_custom_header() {
        let app = App::new("t")
            .global_guard(csrf_guard)
            .module(Module::new().action(&ADD, |_, i: Add| Ok(Total { total: i.by })))
            .finish();
        let no = LocalClient { app: &app, headers: vec![] };
        assert!(matches!(run(&no, &ADD, 1), Err(ActionError::Server { status: 403, .. })));
        let yes = LocalClient { app: &app, headers: vec![("X-Reaxt".into(), "1".into())] };
        assert_eq!(run(&yes, &ADD, 7), Ok(Total { total: 7 }));
    }

    #[test]
    fn malformed_and_oversized_inputs_are_rejected_by_rs_json() {
        let app = app();
        let post = |b: &str| app.dispatch(&reaxt::app::Request { method: "POST", path: "/demo.v1.Counter/Add", headers: &[("x-key".into(), "k".into())], body: b });
        assert_eq!(post("{\"by\":\"x\"}").status, 400);
        assert_eq!(post("not json").status, 400);
        let deep = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        assert_eq!(post(&deep).status, 400, "深さ上限");
    }

    #[test]
    fn unit_wire_is_null_only() {
        assert_eq!(().to_wire(), "null");
        assert_eq!(decode::<()>("null"), Ok(()));
        assert!(decode::<()>("0").is_err());
        assert!(decode::<()>("{}").is_err());
        assert!(decode::<()>("false").is_err());
        assert_eq!(decode::<Option<()>>("null"), Ok(None));
    }

    #[test]
    fn floats_accept_integer_and_exponent_literals_and_box_gql_type_is_transparent() {
        assert_eq!(decode::<f64>("3").unwrap(), 3.0);
        assert_eq!(decode::<f64>("1e2").unwrap(), 100.0);
        assert_eq!(decode::<f32>("-4").unwrap(), -4.0f32);
        assert!(decode::<f64>("null").is_err());
        assert!(decode::<f64>("true").is_err());
        assert_eq!(
            <Box<Vec<Option<i64>>> as Wire>::gql_type(),
            <Vec<Option<i64>> as Wire>::gql_type()
        );
        assert_eq!(
            <Option<Box<i64>> as Wire>::gql_type(),
            GqlType::Nullable(Box::new(GqlType::Int))
        );
    }

    #[test]
    fn vec_of_tuples_round_trips_and_one_bad_element_fails_the_whole_list() {
        let v: Vec<(String, i64)> = decode("[[\"a\",1],[\"b\",-2]]").unwrap();
        assert_eq!(v, vec![("a".to_string(), 1), ("b".to_string(), -2)]);
        assert_eq!(v.to_wire(), "[[\"a\",1],[\"b\",-2]]");
        assert!(decode::<Vec<(String, i64)>>("[[\"a\",1],[\"b\"]]").is_err());
        assert!(decode::<Vec<i64>>("[1,\"x\",3]").is_err());
    }

    #[test]
    fn map_of_tuples_and_options_round_trips_with_sorted_keys() {
        use std::collections::BTreeMap;
        let m: BTreeMap<String, Option<(String, i64)>> =
            decode("{\"z\":[\"a\",1],\"b\":null}").unwrap();
        assert_eq!(m.get("z"), Some(&Some(("a".to_string(), 1))));
        assert_eq!(m.get("b"), Some(&None));
        assert_eq!(m.to_wire(), "{\"b\":null,\"z\":[\"a\",1]}");
        assert!(decode::<BTreeMap<String, (String, i64)>>("{\"k\":[\"a\"]}").is_err());
    }
}
