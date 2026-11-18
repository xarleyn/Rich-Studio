// Pinned, locally served documentation bundles. No CDN, inline script or external validator.
(() => {
  const swagger = document.getElementById('swagger-ui');
  if (swagger) {
    const SwaggerUIBundle = Reflect.get(window, 'SwaggerUIBundle');
    const ui = SwaggerUIBundle({
      url: '/openapi.json',
      dom_id: '#swagger-ui',
      deepLinking: true,
      displayRequestDuration: true,
      validatorUrl: null,
      queryConfigEnabled: false,
      persistAuthorization: false,
      presets: [SwaggerUIBundle.presets.apis],
      requestInterceptor: request => {
        request.credentials = 'same-origin';
        const headers = (request.headers ||= {});
        if (!Object.keys(headers).some((key) => key.toLowerCase() === 'x-studio-client')) headers['X-Studio-Client'] = 'docs';
        return request;
      },
    });
    Reflect.set(window, 'ui', ui);
  }
  const redoc = document.getElementById('redoc');
  if (redoc) {
    const Redoc = Reflect.get(window, 'Redoc');
    Redoc.init('/openapi.json', { disableGoogleFont: true }, redoc);
  }
})();
