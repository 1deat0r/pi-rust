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
