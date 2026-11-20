# Apollo 相互運用の試験 / Apollo interop test
Reaxt 以外の実装(Apollo Subgraph)を、open-cosmo のゲートウェイの Reaxt サーバー2つと並べて動かす試験です。**この試験だけ Node.js を使います**(ユーザーの許可あり。製品のコードには入れません)。
This runs a non-Reaxt implementation (Apollo Subgraph) next to two Reaxt servers behind the open-cosmo gateway. **Node.js is used for this test only** (the owner allowed it; it is not part of the product code).

```
cd examples/apollo-interop && npm install
# 先に Reaxt の serve / serve_greeter と open-cosmo の open-runo-gateway をビルドしておく
bash e2e.sh
```