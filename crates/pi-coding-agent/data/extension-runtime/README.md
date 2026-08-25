# Embedded extension runtime

This directory vendors the runtime pieces used by the pinned upstream
`jiti/static` loader. The JavaScript sources are from `jiti@2.7.0`, published
under the MIT license, with the package's `dist/jiti.cjs` and `dist/babel.cjs`
files retained verbatim. The wrapper mirrors `lib/jiti-static.mjs` and is
loaded from a per-bridge temporary directory at runtime.

Source metadata:

- package: `jiti`
- version: `2.7.0`
- npm integrity: `sha512-AC/7JofJvZGrrneWNaEnJeOLUx+JlGt7tNa0wZiRPT4MY1wmfKjt2+6O2p2uz2+skll8OZZjMJNqeke7kKbNgQ==`
- package tarball: `https://registry.npmjs.org/jiti/-/jiti-2.7.0.tgz`

## Embedded pi module graph

`pi-runtime-graph.mjs` is a Bun-bundled, ESM graph of the pinned published
packages used by upstream's static extension imports:

- `@earendil-works/pi-coding-agent@0.84.2`
- `@earendil-works/pi-agent-core@0.84.2`
- `@earendil-works/pi-ai@0.84.2`
- `@earendil-works/pi-tui@0.84.2`
- `typebox@1.3.7`

The graph resolves the package dependencies from the coding-agent
`npm-shrinkwrap.json` and was built with Bun 1.4.0 using
`bun build entry.mjs --bundle --format=esm --outfile=graph-bundle.mjs --target=node`.
It is 10,759,929 bytes with SHA-256
`a82bde7cf62fcf75bf4f24acadc4ade6e526931812bc5594252e4bb4be6e4896`.
`pi-runtime-modules.mjs` exposes all 20 upstream specifiers and preserves the
intentional mirror identities for the two package-name families, pi-ai
compatibility roots, and TypeBox/Sinclair entries. The direct package tarballs
are published under MIT licenses; the bundle contains the resolved runtime
dependencies from the pinned shrinkwrap.
