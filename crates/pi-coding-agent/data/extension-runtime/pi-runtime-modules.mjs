import * as graph from "./pi-runtime-graph.mjs";

// These namespace-shaped objects are intentionally created once so mirrored
// package specifiers retain the same exported-object identity in jiti's
// virtualModules map, matching upstream's static-import construction. The
// non-enumerable symbol is only a fixture-visible identity witness.
const withIdentity = (value) => {
  Object.defineProperty(value, Symbol.for("pi-rust:bundled-module-identity"), {
    value: {},
    enumerable: false,
  });
  return value;
};
const codingAgent = withIdentity({ ...graph.codingAgent });
const agentCore = withIdentity({ ...graph.agentCore });
const tui = withIdentity({ ...graph.tui });
const aiProviders = withIdentity({ ...graph.aiProviders });
const aiCompat = withIdentity({ ...graph.aiCompat });
const aiOauth = withIdentity({ ...graph.aiOauth });
const typebox = withIdentity({ ...graph.typebox });
const typeboxCompile = withIdentity({ ...graph.typeboxCompile });
const typeboxValue = withIdentity({ ...graph.typeboxValue });

export const modules = {
  "@earendil-works/pi-coding-agent": codingAgent,
  "@earendil-works/pi-agent-core": agentCore,
  "@earendil-works/pi-tui": tui,
  "@earendil-works/pi-ai/providers/all": aiProviders,
  "@earendil-works/pi-ai/compat": aiCompat,
  "@earendil-works/pi-ai/oauth": aiOauth,
  "@earendil-works/pi-ai": aiCompat,
  "@mariozechner/pi-coding-agent": codingAgent,
  "@mariozechner/pi-agent-core": agentCore,
  "@mariozechner/pi-tui": tui,
  "@mariozechner/pi-ai/providers/all": aiProviders,
  "@mariozechner/pi-ai/compat": aiCompat,
  "@mariozechner/pi-ai/oauth": aiOauth,
  "@mariozechner/pi-ai": aiCompat,
  typebox,
  "typebox/compile": typeboxCompile,
  "typebox/value": typeboxValue,
  "@sinclair/typebox": typebox,
  "@sinclair/typebox/compile": typeboxCompile,
  "@sinclair/typebox/value": typeboxValue,
};

export default modules;
