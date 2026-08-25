import { createRequire } from "node:module";
import _createJiti from "./jiti.cjs";
import _babelTransform from "./babel.cjs";

function onError(error) {
  throw error;
}

const nativeImport = (id) => import(id);

export function createJiti(id, opts = {}) {
  if (!opts.transform) {
    opts = { ...opts, transform: _babelTransform };
  }
  return _createJiti(id, opts, {
    onError,
    nativeImport,
    createRequire,
  });
}

export default createJiti;
