# async-graphql 相互運用の試験 / async-graphql interop test

Rust の async-graphql(Reaxt 以外の実装)のサブグラフを、ゲートウェイ(open-cosmo)の後ろで Reaxt と並べる試験用の例です(ポート8795)。
A test example that runs a Rust async-graphql subgraph (a non-Reaxt implementation) next to Reaxt behind the open-cosmo gateway (port 8795).

```n cargo run --manifest-path examples/async-graphql-interop/Cargo.toml
```n