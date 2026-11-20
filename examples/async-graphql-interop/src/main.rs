use async_graphql::{ComplexObject, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{extract::State, routing::post, Router};

struct Query;
#[Object]
impl Query {
    async fn ag_hello(&self) -> &str { "hello from async-graphql" }
    #[graphql(entity)]
    async fn find_visitor(&self, id: String) -> Visitor { Visitor { id } }
}
#[derive(SimpleObject)]
#[graphql(complex)]
struct Visitor { id: String }
#[ComplexObject]
impl Visitor {
    async fn rust_tag(&self) -> String { format!("rust-{}", self.id) }
}
type S = Schema<Query, EmptyMutation, EmptySubscription>;
async fn h(State(s): State<S>, r: GraphQLRequest) -> GraphQLResponse { s.execute(r.into_inner()).await.into() }
#[tokio::main]
async fn main() {
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription).enable_federation().finish();
    let app = Router::new().route("/", post(h)).with_state(schema);
    let l = tokio::net::TcpListener::bind("127.0.0.1:8795").await.unwrap();
    println!("ag subgraph at 8795");
    axum::serve(l, app).await.unwrap();
}
