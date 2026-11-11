// Pinned, locally served documentation bundles. No CDN, inline script or external validator.
(() => {
  const swagger = document.getElementById('swagger-ui');
  if (swagger) {
    const SwaggerUIBundle = Reflect.get(window, 'SwaggerUIBundle');
    SwaggerUIBundle({
      url: '/openapi.json',
      dom_id: '#swagger-ui',
      deepLinking: true,
      displayRequestDuration: true,
      validatorUrl: null,
      presets: [SwaggerUIBundle.presets.apis],
      requestInterceptor: request => {
        request.credentials = 'same-origin';
        return request;
      },
    });
  }
  const redoc = document.getElementById('redoc');
  if (redoc) {
    const Redoc = Reflect.get(window, 'Redoc');
    Redoc.init('/openapi.json', { disableGoogleFont: true }, redoc);
  }
})();
