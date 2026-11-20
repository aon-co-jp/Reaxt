import { ApolloServer } from '@apollo/server';
import { startStandaloneServer } from '@apollo/server/standalone';
import { buildSubgraphSchema } from '@apollo/subgraph';
import { parse } from 'graphql'; const gql = (s) => parse(s[0]);

const typeDefs = gql`
  extend schema @link(url: "https://specs.apollo.dev/federation/v2.0", import: ["@key"])
  type Query { apolloHello: String }
  type Visitor @key(fields: "id") { id: String! nickname: String }
`;
const resolvers = {
  Query: { apolloHello: () => 'hello from apollo' },
  Visitor: { __resolveReference: (ref) => ({ id: ref.id, nickname: 'nick-' + ref.id }) },
};
const server = new ApolloServer({ schema: buildSubgraphSchema([{ typeDefs, resolvers }]) });
const { url } = await startStandaloneServer(server, { listen: { port: 8794 } });
console.log('apollo subgraph at', url);
