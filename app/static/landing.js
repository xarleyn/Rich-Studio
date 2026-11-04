const navToggle = document.querySelector('.nav-toggle');
const nav = document.querySelector('#site-nav');

if (navToggle instanceof HTMLButtonElement && nav) {
  const setNav = (open) => {
    document.body.classList.toggle('nav-open', open);
    navToggle.setAttribute('aria-expanded', String(open));
    navToggle.setAttribute('aria-label', document.documentElement.lang === 'en'
      ? (open ? 'Close menu' : 'Open menu')
      : (open ? 'Закрыть меню' : 'Открыть меню'));
  };

  navToggle.addEventListener('click', () => {
    setNav(navToggle.getAttribute('aria-expanded') !== 'true');
  });
  nav.querySelectorAll('a').forEach((link) => link.addEventListener('click', () => setNav(false)));
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && navToggle.getAttribute('aria-expanded') === 'true') {
      setNav(false);
      navToggle.focus();
    }
  });
  window.matchMedia('(min-width: 801px)').addEventListener('change', (event) => {
    if (event.matches) setNav(false);
  });
}

const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
if (!reducedMotion && 'IntersectionObserver' in window) {
  document.documentElement.classList.add('motion');
  const revealObserver = new IntersectionObserver((entries, observer) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) return;
      entry.target.classList.add('is-visible');
      observer.unobserve(entry.target);
    });
  }, { threshold: 0.12, rootMargin: '0px 0px 48px' });
  document.querySelectorAll('[data-reveal]').forEach((element) => revealObserver.observe(element));

  document.querySelectorAll('[data-word-reveal]').forEach((element) => {
    const words = (element.textContent || '').trim().split(/\s+/);
    element.textContent = '';
    words.forEach((word, index) => {
      const span = document.createElement('span');
      span.className = 'word';
      span.textContent = word;
      element.append(span);
      if (index < words.length - 1) element.append(document.createTextNode(' '));
    });

    const wordObserver = new IntersectionObserver((entries, observer) => {
      if (!entries.some((entry) => entry.isIntersecting)) return;
      element.querySelectorAll('.word').forEach((word, index) => {
        window.setTimeout(() => word.classList.add('is-visible'), index * 45);
      });
      observer.disconnect();
    }, { threshold: 0.4 });
    wordObserver.observe(element);
  });
}
