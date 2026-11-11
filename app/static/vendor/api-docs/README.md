# API documentation assets

Swagger UI 5.34.1 and ReDoc 2.5.4 replace the CDN assets that FastAPI used for `/docs` and `/redoc`. Only their prebuilt browser bundles, Swagger CSS and licenses are vendored; no build step or production Node dependency is required.

Upstream packages: https://www.npmjs.com/package/swagger-ui-dist/v/5.34.1 and https://www.npmjs.com/package/redoc/v/2.5.4.

Refresh deliberately with `npm pack swagger-ui-dist@<version> --ignore-scripts` and `npm pack redoc@<version> --ignore-scripts`, then copy the selected files and licenses from each tarball. In the ReDoc bundle, replace its single `https://cdn.redoc.ly/redoc/logo-mini.svg` reference with `/static/vendor/api-docs/redoc-logo.svg`; the local SVG is an unchanged copy of that upstream logo. This is the only bundle patch and retains the Redocly attribution. Update this record and run `make check` plus the documentation browser smoke check. Do not use unversioned CDN URLs at runtime.

SHA-256 of the served files (the original unpatched ReDoc bundle has SHA-256 `dcaf76612bc4a3fbcc923a8966dee2f6146a5f32e5ce1b6f02dd60cbbf89500b`):

| File | SHA-256 |
|---|---|
| swagger-ui-bundle.js | 36eeb4d5640b9c036b1d0f854499af1cceebbdb260b790a03a18be5e8488fbf5 |
| swagger-ui.css | c3cfe8248c4b0b89673600fb829924eafbac6de4bc4b2b06f5f5243483074d0a |
| redoc.standalone.js | 058933accbd38ea91bc4363d3f28879844ab3b2f8a757ab4e19184f4861e8d43 |
| redoc-logo.svg | 1dab4315f8de2177b2b2726b29e84a26314c69cd27d692228947c0058793d5a7 |
