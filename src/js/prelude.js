// DOM APIs that are simplest to express in JS itself. Runs once per page,
// after the native bindings are installed.
(function () {
  var NodeFilter = {
    FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
    SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 1, SHOW_ATTRIBUTE: 2, SHOW_TEXT: 4,
    SHOW_CDATA_SECTION: 8, SHOW_PROCESSING_INSTRUCTION: 64, SHOW_COMMENT: 128,
    SHOW_DOCUMENT: 256, SHOW_DOCUMENT_TYPE: 512, SHOW_DOCUMENT_FRAGMENT: 1024
  };
  window.NodeFilter = NodeFilter;

  function accept(w, n) {
    if (!(w.whatToShow & (1 << (n.nodeType - 1)))) return 3;
    var f = w.filter;
    if (!f) return 1;
    return typeof f === 'function' ? f(n) : f.acceptNode(n);
  }

  function TreeWalker(root, what, filter) {
    this.root = root;
    this.whatToShow = what === undefined ? 0xFFFFFFFF : what >>> 0;
    this.filter = filter || null;
    this.currentNode = root;
  }
  var T = TreeWalker.prototype;
  T.parentNode = function () {
    var n = this.currentNode;
    while (n && n !== this.root) {
      n = n.parentNode;
      if (n && accept(this, n) === 1) { this.currentNode = n; return n; }
    }
    return null;
  };
  function child(w, first) {
    for (var n = first ? w.currentNode.firstChild : w.currentNode.lastChild; n;
         n = first ? n.nextSibling : n.previousSibling) {
      if (accept(w, n) === 1) { w.currentNode = n; return n; }
    }
    return null;
  }
  function sibling(w, next) {
    var n = w.currentNode;
    if (n === w.root) return null;
    for (var s = next ? n.nextSibling : n.previousSibling; s; s = next ? s.nextSibling : s.previousSibling) {
      if (accept(w, s) === 1) { w.currentNode = s; return s; }
    }
    return null;
  }
  T.firstChild = function () { return child(this, true); };
  T.lastChild = function () { return child(this, false); };
  T.nextSibling = function () { return sibling(this, true); };
  T.previousSibling = function () { return sibling(this, false); };
  T.nextNode = function () {
    var n = this.currentNode, r = 1;
    for (;;) {
      while (r !== 2 && n.firstChild) {
        n = n.firstChild;
        r = accept(this, n);
        if (r === 1) { this.currentNode = n; return n; }
      }
      var t = n, s = null;
      while (t) {
        if (t === this.root) return null;
        s = t.nextSibling;
        if (s) break;
        t = t.parentNode;
      }
      if (!s) return null;
      n = s;
      r = accept(this, n);
      if (r === 1) { this.currentNode = n; return n; }
    }
  };
  T.previousNode = function () {
    var n = this.currentNode;
    while (n !== this.root) {
      var s = n.previousSibling;
      while (s) {
        n = s;
        var r = accept(this, n);
        while (r !== 2 && n.lastChild) { n = n.lastChild; r = accept(this, n); }
        if (r === 1) { this.currentNode = n; return n; }
        s = n.previousSibling;
      }
      if (n === this.root || !n.parentNode) return null;
      n = n.parentNode;
      if (accept(this, n) === 1) { this.currentNode = n; return n; }
    }
    return null;
  };

  function NodeIterator(root, what, filter) {
    this.root = root;
    this.referenceNode = root;
    this.whatToShow = what === undefined ? 0xFFFFFFFF : what >>> 0;
    this.filter = filter || null;
    this._before = true;
  }
  NodeIterator.prototype.nextNode = function () {
    var w = new TreeWalker(this.root, 0xFFFFFFFF, null);
    var n = this.referenceNode;
    if (this._before && accept(this, n) === 1) { this._before = false; return n; }
    this._before = false;
    w.currentNode = n;
    while ((n = w.nextNode())) {
      if (accept(this, n) === 1) { this.referenceNode = n; return n; }
    }
    return null;
  };
  NodeIterator.prototype.previousNode = function () {
    var w = new TreeWalker(this.root, 0xFFFFFFFF, null);
    w.currentNode = this.referenceNode;
    var n;
    while ((n = w.previousNode())) {
      if (accept(this, n) === 1) { this.referenceNode = n; return n; }
    }
    return null;
  };
  NodeIterator.prototype.detach = function () {};

  window.TreeWalker = TreeWalker;
  window.NodeIterator = NodeIterator;
  document.createTreeWalker = function (root, what, filter) { return new TreeWalker(root, what, filter); };
  document.createNodeIterator = function (root, what, filter) { return new NodeIterator(root, what, filter); };

  // Geometry types.
  function DOMRectReadOnly(x, y, w, h) {
    this.x = +x || 0; this.y = +y || 0; this.width = +w || 0; this.height = +h || 0;
  }
  Object.defineProperties(DOMRectReadOnly.prototype, {
    left: { get: function () { return Math.min(this.x, this.x + this.width); } },
    right: { get: function () { return Math.max(this.x, this.x + this.width); } },
    top: { get: function () { return Math.min(this.y, this.y + this.height); } },
    bottom: { get: function () { return Math.max(this.y, this.y + this.height); } }
  });
  DOMRectReadOnly.prototype.toJSON = function () {
    return { x: this.x, y: this.y, width: this.width, height: this.height, top: this.top, right: this.right, bottom: this.bottom, left: this.left };
  };
  DOMRectReadOnly.fromRect = function (r) { r = r || {}; return new this(r.x, r.y, r.width, r.height); };
  function DOMRect(x, y, w, h) { DOMRectReadOnly.call(this, x, y, w, h); }
  DOMRect.prototype = Object.create(DOMRectReadOnly.prototype);
  DOMRect.prototype.constructor = DOMRect;
  DOMRect.fromRect = DOMRectReadOnly.fromRect;
  function DOMPoint(x, y, z, w) { this.x = +x || 0; this.y = +y || 0; this.z = +z || 0; this.w = w === undefined ? 1 : +w; }
  DOMPoint.fromPoint = function (p) { p = p || {}; return new DOMPoint(p.x, p.y, p.z, p.w); };
  DOMPoint.prototype.toJSON = function () { return { x: this.x, y: this.y, z: this.z, w: this.w }; };
  window.DOMRectReadOnly = DOMRectReadOnly;
  window.DOMRect = DOMRect;
  window.DOMPoint = DOMPoint;
  window.DOMPointReadOnly = DOMPoint;

  // MessageChannel: messages are delivered asynchronously to the other port.
  function MessagePort() { this.onmessage = null; this._other = null; this._l = []; }
  MessagePort.prototype.postMessage = function (data) {
    var other = this._other;
    if (!other) return;
    setTimeout(function () {
      var ev = new MessageEvent('message');
      ev.data = data;
      if (typeof other.onmessage === 'function') other.onmessage(ev);
      other._l.slice().forEach(function (f) { typeof f === 'function' ? f(ev) : f.handleEvent(ev); });
    }, 0);
  };
  MessagePort.prototype.addEventListener = function (t, f) { if (t === 'message' && f) this._l.push(f); };
  MessagePort.prototype.removeEventListener = function (t, f) { var i = this._l.indexOf(f); if (i >= 0) this._l.splice(i, 1); };
  MessagePort.prototype.start = function () {};
  MessagePort.prototype.close = function () { this._other = null; };
  function MessageChannel() {
    this.port1 = new MessagePort();
    this.port2 = new MessagePort();
    this.port1._other = this.port2;
    this.port2._other = this.port1;
  }
  window.MessagePort = MessagePort;
  window.MessageChannel = MessageChannel;

  // Selection (no text selection yet: always empty).
  function Selection() { this.rangeCount = 0; this.isCollapsed = true; this.type = 'None'; this.anchorNode = null; this.focusNode = null; this.anchorOffset = 0; this.focusOffset = 0; }
  Selection.prototype.removeAllRanges = function () {};
  Selection.prototype.addRange = function () {};
  Selection.prototype.empty = function () {};
  Selection.prototype.collapse = function () {};
  Selection.prototype.selectAllChildren = function () {};
  Selection.prototype.getRangeAt = function () { throw new DOMException('IndexSizeError'); };
  Selection.prototype.containsNode = function () { return false; };
  Selection.prototype.toString = function () { return ''; };
  window.Selection = Selection;
  var selection = new Selection();
  window.getSelection = function () { return selection; };
  document.getSelection = window.getSelection;

  window.visualViewport = {
    get width() { return window.innerWidth; },
    get height() { return window.innerHeight; },
    scale: 1, offsetLeft: 0, offsetTop: 0,
    get pageLeft() { return window.scrollX; },
    get pageTop() { return window.scrollY; },
    onresize: null, onscroll: null,
    addEventListener: function () {}, removeEventListener: function () {}
  };

  // Prioritized task scheduling.
  window.scheduler = {
    postTask: function (cb, opts) {
      var delay = (opts && opts.delay) || 0;
      return new Promise(function (resolve, reject) {
        setTimeout(function () { try { resolve(cb()); } catch (e) { reject(e); } }, delay);
      });
    },
    yield: function () { return new Promise(function (r) { setTimeout(r, 0); }); }
  };

  // Trusted Types: policies apply their rules and return plain strings.
  window.trustedTypes = {
    createPolicy: function (name, rules) {
      rules = rules || {};
      var id = function (s) { return s; };
      return {
        name: name,
        createHTML: function (s) { return (rules.createHTML || id).apply(null, arguments); },
        createScript: function (s) { return (rules.createScript || id).apply(null, arguments); },
        createScriptURL: function (s) { return (rules.createScriptURL || id).apply(null, arguments); }
      };
    },
    isHTML: function () { return false; }, isScript: function () { return false; }, isScriptURL: function () { return false; },
    emptyHTML: '', emptyScript: '', defaultPolicy: null,
    getAttributeType: function () { return null; }, getPropertyType: function () { return null; }
  };

  // Interfaces scripts test with instanceof or patch via .prototype; they
  // can't be constructed directly.
  ['NodeList', 'HTMLCollection', 'DOMTokenList', 'NamedNodeMap', 'DOMMatrix', 'DOMMatrixReadOnly', 'Window', 'Location',
   'History', 'Navigator', 'Screen', 'Storage', 'MutationRecord', 'IntersectionObserverEntry', 'ResizeObserverEntry',
   'StyleSheet', 'CSSStyleDeclaration', 'MediaList', 'MediaQueryList', 'StyleSheetList', 'FontFaceSet',
   'XMLHttpRequestEventTarget', 'FileList', 'CanvasRenderingContext2D', 'ImageData', 'Range', 'DOMImplementation',
   'Crypto', 'SubtleCrypto', 'CryptoKey', 'CustomElementRegistry', 'ElementInternals', 'VisualViewport',
   'PerformanceEntry', 'PerformanceMark', 'PerformanceMeasure', 'PerformanceNavigationTiming', 'PerformanceResourceTiming',
   'Animation', 'KeyframeEffect', 'AnimationEffect', 'DocumentTimeline', 'AnimationTimeline', 'TrustedHTML',
   'TrustedScript', 'TrustedScriptURL', 'TrustedTypePolicy', 'Scheduler', 'TaskController', 'Plugin', 'PluginArray',
   'MimeType', 'MimeTypeArray', 'BarProp', 'DOMStringMap', 'DOMStringList', 'ValidityState', 'TimeRanges',
   'TextMetrics', 'CanvasGradient', 'CanvasPattern', 'SVGAnimatedString', 'SVGAnimatedLength', 'SVGLength'
  ].forEach(function (name) {
    if (typeof window[name] !== 'undefined') return;
    var C = function () { throw new TypeError('Illegal constructor'); };
    Object.defineProperty(C, 'name', { value: name });
    window[name] = C;
  });

  // Intl, en-US only: enough for number / date / plural formatting.
  if (typeof Intl === 'undefined') {
    var loc = function (l) { return (Array.isArray(l) ? l[0] : l) || 'en-US'; };
    var group = function (s) { return s.replace(/\B(?=(\d{3})+(?!\d))/g, ','); };
    var fixed = function (n, min, max) {
      var s = n.toFixed(max);
      if (s.indexOf('.') >= 0) {
        var keep = s.length - (max - min);
        while (s.length > keep && s[s.length - 1] === '0') s = s.slice(0, -1);
        if (s[s.length - 1] === '.') s = s.slice(0, -1);
      }
      return s;
    };
    var CUR = { USD: '$', EUR: '€', GBP: '£', JPY: '¥', INR: '₹', CNY: 'CN¥', KRW: '₩', CAD: 'CA$', AUD: 'A$' };
    function NumberFormat(l, o) {
      if (!(this instanceof NumberFormat)) return new NumberFormat(l, o);
      o = o || {};
      this._o = o;
      this._style = o.style || 'decimal';
      var defMax = this._style === 'currency' ? 2 : this._style === 'percent' ? 0 : 3;
      this._min = o.minimumFractionDigits !== undefined ? o.minimumFractionDigits : (this._style === 'currency' ? 2 : 0);
      this._max = Math.max(this._min, o.maximumFractionDigits !== undefined ? o.maximumFractionDigits : defMax);
      this._locale = loc(l);
    }
    NumberFormat.prototype.format = function (n) {
      n = Number(n);
      if (isNaN(n)) return 'NaN';
      if (!isFinite(n)) return n < 0 ? '-∞' : '∞';
      var o = this._o, neg = n < 0 || Object.is(n, -0) && false, suffix = '';
      var v = Math.abs(n);
      if (this._style === 'percent') v *= 100;
      if (o.notation === 'compact') {
        var units = [[1e12, 'T'], [1e9, 'B'], [1e6, 'M'], [1e3, 'K']];
        for (var i = 0; i < units.length; i++) {
          if (v >= units[i][0]) {
            v /= units[i][0];
            suffix = units[i][1];
            break;
          }
        }
        var digits = o.maximumFractionDigits !== undefined ? o.maximumFractionDigits : (v < 100 && suffix ? 1 : 0);
        var str = fixed(v, 0, digits);
        if (suffix && Number(str) >= 1000) { str = '1'; suffix = { K: 'M', M: 'B', B: 'T', T: 'T' }[suffix]; }
        var out = (o.useGrouping === false ? str : group(str.split('.')[0]) + (str.indexOf('.') >= 0 ? '.' + str.split('.')[1] : '')) + suffix;
        return (neg ? '-' : '') + out;
      }
      var s = fixed(v, this._min, this._max);
      if (o.minimumIntegerDigits) {
        var ip = s.split('.')[0];
        while (ip.length < o.minimumIntegerDigits) ip = '0' + ip;
        s = ip + (s.indexOf('.') >= 0 ? '.' + s.split('.')[1] : '');
      }
      var parts = s.split('.');
      var body = (o.useGrouping === false ? parts[0] : group(parts[0])) + (parts[1] ? '.' + parts[1] : '');
      if (this._style === 'percent') body += '%';
      if (this._style === 'currency') {
        var c = (o.currency || 'USD').toUpperCase();
        var sym = o.currencyDisplay === 'code' ? c + ' ' : (CUR[c] || c + ' ');
        body = sym + body;
      }
      if (this._style === 'unit' && o.unit) body += ' ' + o.unit;
      return (neg ? '-' : '') + body;
    };
    NumberFormat.prototype.formatToParts = function (n) { return [{ type: 'literal', value: this.format(n) }]; };
    NumberFormat.prototype.resolvedOptions = function () {
      return { locale: this._locale, numberingSystem: 'latn', style: this._style, minimumFractionDigits: this._min, maximumFractionDigits: this._max, useGrouping: this._o.useGrouping !== false, notation: this._o.notation || 'standard', currency: this._o.currency };
    };
    NumberFormat.supportedLocalesOf = function (l) { return [].concat(l || []); };

    var MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
    var DAYS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
    var pad2 = function (n) { return (n < 10 ? '0' : '') + n; };
    function DateTimeFormat(l, o) {
      if (!(this instanceof DateTimeFormat)) return new DateTimeFormat(l, o);
      o = Object.assign({}, o || {});
      if (o.dateStyle || o.timeStyle) {
        if (o.dateStyle === 'full') { o.weekday = 'long'; o.month = 'long'; o.day = 'numeric'; o.year = 'numeric'; }
        else if (o.dateStyle === 'long') { o.month = 'long'; o.day = 'numeric'; o.year = 'numeric'; }
        else if (o.dateStyle === 'medium') { o.month = 'short'; o.day = 'numeric'; o.year = 'numeric'; }
        else if (o.dateStyle) { o.month = 'numeric'; o.day = 'numeric'; o.year = '2-digit'; }
        if (o.timeStyle) { o.hour = 'numeric'; o.minute = '2-digit'; if (o.timeStyle !== 'short') o.second = '2-digit'; }
      }
      if (!o.weekday && !o.year && !o.month && !o.day && !o.hour && !o.minute && !o.second) {
        o.year = 'numeric'; o.month = 'numeric'; o.day = 'numeric';
      }
      this._o = o;
      this._locale = loc(l);
    }
    DateTimeFormat.prototype.formatToParts = function (d) {
      d = d === undefined ? new Date() : new Date(d);
      var o = this._o, parts = [];
      var lit = function (v) { parts.push({ type: 'literal', value: v }); };
      var utc = o.timeZone === 'UTC';
      var Y = utc ? d.getUTCFullYear() : d.getFullYear(), M = utc ? d.getUTCMonth() : d.getMonth(), D = utc ? d.getUTCDate() : d.getDate();
      var W = utc ? d.getUTCDay() : d.getDay(), h = utc ? d.getUTCHours() : d.getHours(), m = utc ? d.getUTCMinutes() : d.getMinutes(), s = utc ? d.getUTCSeconds() : d.getSeconds();
      var textMonth = o.month === 'long' || o.month === 'short' || o.month === 'narrow';
      if (o.weekday) { parts.push({ type: 'weekday', value: o.weekday === 'long' ? DAYS[W] : DAYS[W].slice(0, o.weekday === 'narrow' ? 1 : 3) }); if (o.month || o.day || o.year) lit(', '); }
      if (textMonth) {
        parts.push({ type: 'month', value: o.month === 'long' ? MONTHS[M] : MONTHS[M].slice(0, o.month === 'narrow' ? 1 : 3) });
        if (o.day) { lit(' '); parts.push({ type: 'day', value: o.day === '2-digit' ? pad2(D) : String(D) }); }
        if (o.year) { lit(o.day ? ', ' : ' '); parts.push({ type: 'year', value: o.year === '2-digit' ? pad2(Y % 100) : String(Y) }); }
      } else {
        var dp = [];
        if (o.month) dp.push({ type: 'month', value: o.month === '2-digit' ? pad2(M + 1) : String(M + 1) });
        if (o.day) dp.push({ type: 'day', value: o.day === '2-digit' ? pad2(D) : String(D) });
        if (o.year) dp.push({ type: 'year', value: o.year === '2-digit' ? pad2(Y % 100) : String(Y) });
        dp.forEach(function (p, i) { if (i) lit('/'); parts.push(p); });
      }
      if (o.hour || o.minute || o.second) {
        if (parts.length) lit(', ');
        var h12 = o.hour12 !== false && o.hourCycle !== 'h23' && o.hourCycle !== 'h24';
        if (o.hour) parts.push({ type: 'hour', value: h12 ? String(h % 12 || 12) : (o.hour === '2-digit' ? pad2(h) : String(h)) });
        if (o.minute) { if (o.hour) lit(':'); parts.push({ type: 'minute', value: pad2(m) }); }
        if (o.second) { lit(':'); parts.push({ type: 'second', value: pad2(s) }); }
        if (o.hour && h12) { lit(' '); parts.push({ type: 'dayPeriod', value: h < 12 ? 'AM' : 'PM' }); }
      }
      return parts;
    };
    DateTimeFormat.prototype.format = function (d) { return this.formatToParts(d).map(function (p) { return p.value; }).join(''); };
    DateTimeFormat.prototype.formatRange = function (a, b) { return this.format(a) + ' – ' + this.format(b); };
    DateTimeFormat.prototype.resolvedOptions = function () {
      return Object.assign({ locale: this._locale, calendar: 'gregory', numberingSystem: 'latn', timeZone: this._o.timeZone || 'UTC' }, this._o);
    };
    DateTimeFormat.supportedLocalesOf = NumberFormat.supportedLocalesOf;

    function PluralRules(l, o) { if (!(this instanceof PluralRules)) return new PluralRules(l, o); this._type = (o && o.type) || 'cardinal'; this._locale = loc(l); }
    PluralRules.prototype.select = function (n) {
      n = Number(n);
      if (this._type === 'ordinal') {
        var t = n % 10, c = n % 100;
        if (t === 1 && c !== 11) return 'one';
        if (t === 2 && c !== 12) return 'two';
        if (t === 3 && c !== 13) return 'few';
        return 'other';
      }
      return n === 1 ? 'one' : 'other';
    };
    PluralRules.prototype.resolvedOptions = function () { return { locale: this._locale, type: this._type, pluralCategories: ['one', 'other'] }; };
    PluralRules.supportedLocalesOf = NumberFormat.supportedLocalesOf;

    function Collator(l, o) { if (!(this instanceof Collator)) return new Collator(l, o); this._o = o || {}; var self = this; this.compare = function (a, b) { return self._cmp(a, b); }; }
    Collator.prototype._cmp = function (a, b) {
      a = String(a); b = String(b);
      if (this._o.sensitivity === 'base' || this._o.sensitivity === 'accent') { a = a.toLowerCase(); b = b.toLowerCase(); }
      if (this._o.numeric) {
        var na = parseFloat(a), nb = parseFloat(b);
        if (!isNaN(na) && !isNaN(nb) && na !== nb) return na < nb ? -1 : 1;
      }
      return a < b ? -1 : a > b ? 1 : 0;
    };
    Collator.prototype.resolvedOptions = function () { return Object.assign({ locale: 'en-US', usage: 'sort', sensitivity: 'variant' }, this._o); };
    Collator.supportedLocalesOf = NumberFormat.supportedLocalesOf;

    function RelativeTimeFormat(l, o) { if (!(this instanceof RelativeTimeFormat)) return new RelativeTimeFormat(l, o); this._o = o || {}; }
    RelativeTimeFormat.prototype.format = function (v, unit) {
      v = Number(v);
      unit = String(unit).replace(/s$/, '');
      if (this._o.numeric === 'auto') {
        if (unit === 'day' && v === 0) return 'today';
        if (unit === 'day' && v === 1) return 'tomorrow';
        if (unit === 'day' && v === -1) return 'yesterday';
        if (v === 0) return 'this ' + unit;
        if (v === 1) return 'next ' + unit;
        if (v === -1) return 'last ' + unit;
      }
      var a = Math.abs(v), u = unit + (a === 1 ? '' : 's');
      return v < 0 || Object.is(v, -0) ? a + ' ' + u + ' ago' : 'in ' + a + ' ' + u;
    };
    RelativeTimeFormat.prototype.formatToParts = function (v, u) { return [{ type: 'literal', value: this.format(v, u) }]; };
    RelativeTimeFormat.prototype.resolvedOptions = function () { return Object.assign({ locale: 'en-US', style: 'long', numeric: 'always' }, this._o); };

    function ListFormat(l, o) { if (!(this instanceof ListFormat)) return new ListFormat(l, o); this._o = o || {}; }
    ListFormat.prototype.format = function (list) {
      var a = Array.from(list, String), word = this._o.type === 'disjunction' ? 'or' : 'and';
      if (a.length < 2) return a.join('');
      if (a.length === 2) return a[0] + ' ' + word + ' ' + a[1];
      return a.slice(0, -1).join(', ') + ', ' + word + ' ' + a[a.length - 1];
    };

    function Segmenter(l, o) { if (!(this instanceof Segmenter)) return new Segmenter(l, o); this._g = (o && o.granularity) || 'grapheme'; }
    Segmenter.prototype.segment = function (input) {
      input = String(input);
      var segs = [], idx = 0;
      var pieces = this._g === 'grapheme' ? Array.from(input) : this._g === 'word' ? (input.match(/\w+|\s+|[^\w\s]+/g) || []) : (input.match(/[^.!?]+[.!?]*\s*/g) || []);
      pieces.forEach(function (p) {
        var s = { segment: p, index: idx, input: input };
        if (this._g === 'word') s.isWordLike = /\w/.test(p);
        segs.push(s);
        idx += p.length;
      }, this);
      segs.containing = function (i) { for (var k = 0; k < segs.length; k++) if (i >= segs[k].index && i < segs[k].index + segs[k].segment.length) return segs[k]; };
      return segs;
    };

    function DisplayNames(l, o) { this._o = o || {}; }
    DisplayNames.prototype.of = function (code) { return code; };

    function Locale(tag) { this.baseName = String(tag); var p = this.baseName.split('-'); this.language = p[0]; this.region = p[1]; }
    Locale.prototype.toString = function () { return this.baseName; };
    Locale.prototype.maximize = function () { return this; };
    Locale.prototype.minimize = function () { return this; };

    window.Intl = {
      NumberFormat: NumberFormat, DateTimeFormat: DateTimeFormat, PluralRules: PluralRules, Collator: Collator,
      RelativeTimeFormat: RelativeTimeFormat, ListFormat: ListFormat, Segmenter: Segmenter, DisplayNames: DisplayNames,
      Locale: Locale,
      getCanonicalLocales: function (l) { return [].concat(l || []); },
      supportedValuesOf: function () { return []; }
    };
  }

  // Objects are never collected while reachable from here, so a WeakRef
  // always derefs and finalizers never run.
  if (typeof WeakRef === 'undefined') {
    window.WeakRef = function WeakRef(target) {
      if (target === null || (typeof target !== 'object' && typeof target !== 'function')) throw new TypeError('WeakRef: invalid target');
      this._t = target;
    };
    WeakRef.prototype.deref = function () { return this._t; };
    window.FinalizationRegistry = function FinalizationRegistry(cb) { this._cb = cb; };
    FinalizationRegistry.prototype.register = function () {};
    FinalizationRegistry.prototype.unregister = function () { return false; };
  }

  // Font loading: web fonts are fetched by the engine, so script-side font
  // loads resolve immediately.
  function FontFace(family, source, desc) {
    this.family = family;
    this.source = source;
    this.status = 'loaded';
    desc = desc || {};
    this.style = desc.style || 'normal';
    this.weight = desc.weight || 'normal';
    this.display = desc.display || 'auto';
    this.loaded = Promise.resolve(this);
  }
  FontFace.prototype.load = function () { return this.loaded; };
  window.FontFace = FontFace;
  var faces = [];
  var fonts = {
    status: 'loaded',
    ready: null,
    onloading: null, onloadingdone: null, onloadingerror: null,
    load: function () { return Promise.resolve([]); },
    check: function () { return true; },
    add: function (f) { if (faces.indexOf(f) < 0) faces.push(f); return this; },
    delete: function (f) { var i = faces.indexOf(f); if (i < 0) return false; faces.splice(i, 1); return true; },
    has: function (f) { return faces.indexOf(f) >= 0; },
    clear: function () { faces.length = 0; },
    forEach: function (cb, t) { faces.slice().forEach(function (f) { cb.call(t, f, f, fonts); }); },
    values: function () { return faces.slice()[Symbol.iterator](); },
    addEventListener: function () {},
    removeEventListener: function () {},
    get size() { return faces.length; }
  };
  fonts.ready = Promise.resolve(fonts);
  fonts[Symbol.iterator] = fonts.values;
  document.fonts = fonts;
})();
