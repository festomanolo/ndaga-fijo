// Reveal-on-scroll, benchmark bars, copy chip. Everything works without JS (content is visible by default via <noscript> style).
(function () {
  var els = document.querySelectorAll('.rv,.bench');
  if (!('IntersectionObserver' in window)) { els.forEach(function (e) { e.classList.add('in'); }); }
  else {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) { if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); } });
    }, { threshold: 0.18 });
    els.forEach(function (e) { io.observe(e); });
  }
  document.querySelectorAll('[data-copy]').forEach(function (b) {
    b.addEventListener('click', function () {
      var t = b.getAttribute('data-copy'), s = b.querySelector('span');
      (navigator.clipboard ? navigator.clipboard.writeText(t) : Promise.reject()).then(function () {
        if (s) { var o = s.textContent; s.textContent = 'copied'; setTimeout(function () { s.textContent = o; }, 1400); }
      }).catch(function () {});
    });
  });
})();
