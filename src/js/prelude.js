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

  // Range: boundary points over the DOM, with the commonly used operations.
  function nodeLength(n) {
    return n.nodeType === 3 || n.nodeType === 8 ? n.data.length : n.childNodes.length;
  }
  function indexIn(n) { var i = 0; while ((n = n.previousSibling)) i++; return i; }
  function Range(doc) {
    var d = doc || document;
    this.startContainer = d; this.startOffset = 0;
    this.endContainer = d; this.endOffset = 0;
  }
  var R = Range.prototype;
  Object.defineProperty(R, 'collapsed', { get: function () { return this.startContainer === this.endContainer && this.startOffset === this.endOffset; } });
  Object.defineProperty(R, 'commonAncestorContainer', { get: function () {
    var a = this.startContainer;
    while (a && !(a === this.endContainer || a.contains(this.endContainer))) a = a.parentNode;
    return a || this.startContainer;
  } });
  R.setStart = function (n, o) { this.startContainer = n; this.startOffset = o; if (this.endContainer === n && this.endOffset < o || !this.endContainer) { this.endContainer = n; this.endOffset = o; } };
  R.setEnd = function (n, o) { this.endContainer = n; this.endOffset = o; };
  R.setStartBefore = function (n) { this.setStart(n.parentNode, indexIn(n)); };
  R.setStartAfter = function (n) { this.setStart(n.parentNode, indexIn(n) + 1); };
  R.setEndBefore = function (n) { this.setEnd(n.parentNode, indexIn(n)); };
  R.setEndAfter = function (n) { this.setEnd(n.parentNode, indexIn(n) + 1); };
  R.selectNode = function (n) { this.setStartBefore(n); this.setEndAfter(n); };
  R.selectNodeContents = function (n) { this.startContainer = n; this.startOffset = 0; this.endContainer = n; this.endOffset = nodeLength(n); };
  R.collapse = function (toStart) {
    if (toStart) { this.endContainer = this.startContainer; this.endOffset = this.startOffset; }
    else { this.startContainer = this.endContainer; this.startOffset = this.endOffset; }
  };
  R.cloneRange = function () {
    var r = new Range();
    r.startContainer = this.startContainer; r.startOffset = this.startOffset;
    r.endContainer = this.endContainer; r.endOffset = this.endOffset;
    return r;
  };
  R.detach = function () {};
  // The top-level nodes fully inside the range when both ends share a parent.
  R._nodes = function () {
    if (this.startContainer !== this.endContainer || this.startContainer.nodeType === 3) return null;
    return Array.prototype.slice.call(this.startContainer.childNodes, this.startOffset, this.endOffset);
  };
  R.toString = function () {
    var s = this.startContainer, e = this.endContainer;
    if (s === e && (s.nodeType === 3 || s.nodeType === 8)) return s.data.slice(this.startOffset, this.endOffset);
    var nodes = this._nodes();
    if (nodes) return nodes.map(function (n) { return n.textContent; }).join('');
    return (this.commonAncestorContainer.textContent || '');
  };
  R.createContextualFragment = function (html) {
    var t = document.createElement('template');
    t.innerHTML = html;
    return document.importNode(t.content, true);
  };
  R.deleteContents = function () {
    var s = this.startContainer;
    if (s === this.endContainer && s.nodeType === 3) { s.data = s.data.slice(0, this.startOffset) + s.data.slice(this.endOffset); this.collapse(true); return; }
    var nodes = this._nodes();
    if (nodes) nodes.forEach(function (n) { n.parentNode.removeChild(n); });
    this.collapse(true);
  };
  R.extractContents = function () {
    var f = document.createDocumentFragment(), s = this.startContainer;
    if (s === this.endContainer && s.nodeType === 3) {
      f.appendChild(document.createTextNode(s.data.slice(this.startOffset, this.endOffset)));
      this.deleteContents();
      return f;
    }
    var nodes = this._nodes();
    if (nodes) nodes.forEach(function (n) { f.appendChild(n); });
    this.collapse(true);
    return f;
  };
  R.cloneContents = function () {
    var f = document.createDocumentFragment(), s = this.startContainer;
    if (s === this.endContainer && s.nodeType === 3) { f.appendChild(document.createTextNode(s.data.slice(this.startOffset, this.endOffset))); return f; }
    var nodes = this._nodes();
    if (nodes) nodes.forEach(function (n) { f.appendChild(n.cloneNode(true)); });
    return f;
  };
  R.insertNode = function (n) {
    var c = this.startContainer;
    if (c.nodeType === 3) {
      var after = c.splitText ? c.splitText(this.startOffset) : null;
      c.parentNode.insertBefore(n, after || c.nextSibling);
    } else {
      c.insertBefore(n, c.childNodes[this.startOffset] || null);
    }
  };
  R.surroundContents = function (el) { el.appendChild(this.extractContents()); this.insertNode(el); this.selectNode(el); };
  R.compareBoundaryPoints = function () { return 0; };
  R.getBoundingClientRect = function () {
    var c = this.startContainer;
    var el = c.nodeType === 1 ? c : c.parentNode;
    var nodes = this._nodes();
    if (nodes) {
      var rects = nodes.filter(function (n) { return n.nodeType === 1; }).map(function (n) { return n.getBoundingClientRect(); });
      if (rects.length) {
        var l = Math.min.apply(0, rects.map(function (r) { return r.left; })), t = Math.min.apply(0, rects.map(function (r) { return r.top; }));
        var r_ = Math.max.apply(0, rects.map(function (r) { return r.right; })), b = Math.max.apply(0, rects.map(function (r) { return r.bottom; }));
        return new DOMRect(l, t, r_ - l, b - t);
      }
    }
    return el && el.getBoundingClientRect ? el.getBoundingClientRect() : new DOMRect();
  };
  R.getClientRects = function () { return [this.getBoundingClientRect()]; };
  Range.START_TO_START = 0; Range.START_TO_END = 1; Range.END_TO_END = 2; Range.END_TO_START = 3;
  window.Range = Range;
  Node.prototype.createRange = function () { return new Range(this.nodeType === 9 ? this : this.ownerDocument); };

  // Interfaces scripts test with instanceof or patch via .prototype; they
  // can't be constructed directly.
  ['NodeList', 'HTMLCollection', 'DOMTokenList', 'NamedNodeMap', 'DOMMatrix', 'DOMMatrixReadOnly', 'Location',
   'History', 'Navigator', 'Screen', 'Storage', 'MutationRecord', 'IntersectionObserverEntry', 'ResizeObserverEntry',
   'StyleSheet', 'CSSStyleDeclaration', 'MediaList', 'MediaQueryList', 'StyleSheetList', 'FontFaceSet',
   'XMLHttpRequestEventTarget', 'FileList', 'CanvasRenderingContext2D', 'ImageData', 'DOMImplementation',
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

  // Cache Storage, in memory for the page's lifetime.
  if (typeof caches === 'undefined') {
    var keyOf = function (r) { return typeof r === 'string' ? new URL(r, location.href).href : r && r.url; };
    function Cache() { this._m = new Map(); }
    Cache.prototype.match = function (r) { var e = this._m.get(keyOf(r)); return Promise.resolve(e ? e.res.clone() : undefined); };
    Cache.prototype.matchAll = function (r) {
      var out = [];
      this._m.forEach(function (e, k) { if (r === undefined || k === keyOf(r)) out.push(e.res.clone()); });
      return Promise.resolve(out);
    };
    Cache.prototype.put = function (r, res) { this._m.set(keyOf(r), { req: r, res: res }); return Promise.resolve(); };
    Cache.prototype.add = function (r) { var self = this; return fetch(r).then(function (res) { return self.put(r, res); }); };
    Cache.prototype.addAll = function (rs) { var self = this; return Promise.all(rs.map(function (r) { return self.add(r); })).then(function () {}); };
    Cache.prototype['delete'] = function (r) { return Promise.resolve(this._m['delete'](keyOf(r))); };
    Cache.prototype.keys = function () { var out = []; this._m.forEach(function (e, k) { out.push(new Request(k)); }); return Promise.resolve(out); };
    function CacheStorage() { this._c = new Map(); }
    CacheStorage.prototype.open = function (n) { if (!this._c.has(n)) this._c.set(n, new Cache()); return Promise.resolve(this._c.get(n)); };
    CacheStorage.prototype.has = function (n) { return Promise.resolve(this._c.has(n)); };
    CacheStorage.prototype['delete'] = function (n) { return Promise.resolve(this._c['delete'](n)); };
    CacheStorage.prototype.keys = function () { return Promise.resolve(Array.from(this._c.keys())); };
    CacheStorage.prototype.match = function (r) {
      var all = Array.from(this._c.values());
      return all.reduce(function (p, c) { return p.then(function (hit) { return hit || c.match(r); }); }, Promise.resolve(undefined));
    };
    window.Cache = Cache;
    window.CacheStorage = CacheStorage;
    window.caches = new CacheStorage();
  }

  // DOM properties as accessors on Node.prototype (as in browsers), so
  // Object.getOwnPropertyDescriptor finds them; polyfills (Shady DOM) wrap
  // these. They call into the native DOM layer.
  (function () {
    var nget = window['@@nativeGet'], nset = window['@@nativeSet'];
    var NP = Node.prototype;
    ['firstChild', 'lastChild', 'nextSibling', 'previousSibling', 'parentNode', 'parentElement', 'childNodes',
     'children', 'firstElementChild', 'lastElementChild', 'nextElementSibling', 'previousElementSibling',
     'childElementCount', 'textContent', 'innerHTML', 'outerHTML', 'nodeType', 'nodeName', 'nodeValue',
     'ownerDocument', 'isConnected', 'baseURI', 'id', 'className', 'classList', 'tagName', 'localName',
     'namespaceURI', 'attributes', 'shadowRoot', 'slot', 'assignedSlot', 'innerText', 'hidden', 'title',
     'lang', 'dir', 'style', 'dataset', 'host', 'value', 'checked', 'disabled', 'href', 'src', 'name', 'type',
     'data', 'tabIndex', 'contentEditable', 'isContentEditable', 'offsetParent', 'offsetWidth', 'offsetHeight',
     'offsetLeft', 'offsetTop', 'clientWidth', 'clientHeight', 'scrollWidth', 'scrollHeight', 'scrollTop',
     'scrollLeft', 'activeElement', 'action', 'method', 'target', 'rel', 'alt', 'placeholder', 'selected',
     'multiple', 'required', 'readOnly', 'min', 'max', 'step', 'pattern', 'form', 'label', 'htmlFor', 'width', 'height'
    ].forEach(function (k) {
      if (Object.getOwnPropertyDescriptor(NP, k)) return;
      var g = function () { return nget(this, k); };
      var st = function (v) { nset(this, k, v); };
      Object.defineProperty(g, '@@nativeAccessor', { value: true });
      Object.defineProperty(st, '@@nativeAccessor', { value: true });
      Object.defineProperty(NP, k, { get: g, set: st, configurable: true, enumerable: true });
    });
  })();

  // Media elements: no playback, but the API exists and behaves like a
  // paused, never-loaded video (so players don't crash; posters show).
  (function () {
    var NP = Node.prototype, media = { video: 1, audio: 1 };
    var isMedia = function (el) { return el && el.nodeType === 1 && media[el.localName]; };
    var state = function (el) {
      return el.__media || (el.__media = { currentTime: 0, paused: true, muted: el.hasAttribute('muted'), volume: 1, playbackRate: 1, ended: false });
    };
    var methods = {
      load: function () {},
      play: function () {
        var el = this;
        state(el);
        return Promise.reject(new DOMException('The element has no supported sources.', 'NotSupportedError'));
      },
      pause: function () { state(this).paused = true; },
      canPlayType: function () { return ''; },
      addTextTrack: function () { return { mode: 'disabled', cues: [], addCue: function () {} }; },
      fastSeek: function (t) { state(this).currentTime = t; },
      requestPictureInPicture: function () { return Promise.reject(new DOMException('Not supported', 'NotSupportedError')); },
      captureStream: function () { return null; }
    };
    Object.keys(methods).forEach(function (k) {
      if (!(k in NP)) Object.defineProperty(NP, k, { value: function () { return isMedia(this) ? methods[k].apply(this, arguments) : undefined; }, configurable: true, writable: true });
    });
    var props = {
      currentTime: [function () { return state(this).currentTime; }, function (v) { state(this).currentTime = +v || 0; }],
      paused: [function () { return state(this).paused; }],
      ended: [function () { return false; }],
      muted: [function () { return state(this).muted; }, function (v) { state(this).muted = !!v; }],
      volume: [function () { return state(this).volume; }, function (v) { state(this).volume = +v; }],
      playbackRate: [function () { return state(this).playbackRate; }, function (v) { state(this).playbackRate = +v; }],
      duration: [function () { return NaN; }],
      readyState: [function () { return 0; }],
      networkState: [function () { return 3; }],
      buffered: [function () { return { length: 0, start: function () { return 0; }, end: function () { return 0; } }; }],
      seekable: [function () { return { length: 0, start: function () { return 0; }, end: function () { return 0; } }; }],
      played: [function () { return { length: 0 }; }],
      videoWidth: [function () { return 0; }],
      videoHeight: [function () { return 0; }],
      currentSrc: [function () { return this.src || ''; }],
      textTracks: [function () { return []; }]
    };
    var nget = window['@@nativeGet'], nset = window['@@nativeSet'];
    Object.keys(props).forEach(function (k) {
      if (Object.getOwnPropertyDescriptor(NP, k)) return;
      var g = props[k][0], s = props[k][1];
      // Marked native: the DOM layer answers first (document.readyState...);
      // these only serve media elements.
      var get = function () { return isMedia(this) ? g.call(this) : nget(this, k); };
      var set = function (v) { if (isMedia(this) && s) s.call(this, v); else nset(this, k, v); };
      Object.defineProperty(get, '@@nativeAccessor', { value: true });
      Object.defineProperty(set, '@@nativeAccessor', { value: true });
      Object.defineProperty(NP, k, { get: get, set: set, configurable: true });
    });
    ['HAVE_NOTHING', 'HAVE_METADATA', 'HAVE_CURRENT_DATA', 'HAVE_FUTURE_DATA', 'HAVE_ENOUGH_DATA'].forEach(function (k, i) {
      if (window.HTMLMediaElement) HTMLMediaElement[k] = i;
    });
  })();

  // Iframes: we don't load frame documents; contentWindow is an inert,
  // about:blank-like window (cross-origin pages see much the same).
  (function () {
    var NP = Node.prototype;
    if (Object.getOwnPropertyDescriptor(NP, 'contentWindow')) return;
    var frameWindow = function (el) {
      if (el.__frameWindow) return el.__frameWindow;
      // Built-ins (JSON, Object, ...) come from this window: scripts borrow
      // a frame's "pristine" globals.
      var w = Object.assign(Object.create(window), {
        location: { href: 'about:blank', origin: 'null', protocol: 'about:', host: '', hostname: '', pathname: 'blank', search: '', hash: '', replace: function () {}, assign: function () {}, reload: function () {} },
        postMessage: function (data, origin, transfer) { window['@@framePost'](el, data, origin, transfer); },
        addEventListener: function () {}, removeEventListener: function () {},
        focus: function () {}, blur: function () {}, close: function () {}, closed: false,
        parent: window, top: window, frames: [], length: 0, opener: null
      });
      w.window = w; w.self = w;
      el.__frameWindow = w;
      return w;
    };
    Object.defineProperty(NP, 'contentWindow', { get: function () { return this.localName === 'iframe' || this.localName === 'frame' || this.localName === 'object' ? frameWindow(this) : undefined; }, configurable: true });
    Object.defineProperty(NP, 'contentDocument', { get: function () { return this.localName === 'iframe' || this.localName === 'frame' || this.localName === 'object' ? null : undefined; }, configurable: true });
  })();

  // Newer ECMAScript library functions.
  var def = function (o, name, f) { if (!(name in o)) Object.defineProperty(o, name, { value: f, writable: true, configurable: true, enumerable: false }); };
  def(Object, 'hasOwn', function (o, k) { return Object.prototype.hasOwnProperty.call(o, k); });
  def(Object, 'getOwnPropertyDescriptors', function (o) {
    var out = {};
    var keys = Object.getOwnPropertyNames(o).concat(Object.getOwnPropertySymbols ? Object.getOwnPropertySymbols(o) : []);
    keys.forEach(function (k) { var d = Object.getOwnPropertyDescriptor(o, k); if (d) out[k] = d; });
    return out;
  });
  def(Object, 'groupBy', function (items, f) {
    var out = Object.create(null), i = 0;
    for (var x of items) { var k = f(x, i++); (out[k] || (out[k] = [])).push(x); }
    return out;
  });
  def(Map, 'groupBy', function (items, f) {
    var out = new Map(), i = 0;
    for (var x of items) { var k = f(x, i++); if (!out.has(k)) out.set(k, []); out.get(k).push(x); }
    return out;
  });
  def(Promise, 'withResolvers', function () {
    var r = {};
    r.promise = new Promise(function (res, rej) { r.resolve = res; r.reject = rej; });
    return r;
  });
  def(Array, 'fromAsync', async function (items, f) {
    var out = [], i = 0;
    if (items[Symbol.asyncIterator]) { for await (var x of items) out.push(f ? await f(x, i++) : x); }
    else { for (var y of Array.from(items)) { var v = await y; out.push(f ? await f(v, i++) : v); } }
    return out;
  });
  def(String.prototype, 'isWellFormed', function () { return !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(^|[^\uD800-\uDBFF])[\uDC00-\uDFFF]/.test(this); });
  def(String.prototype, 'toWellFormed', function () { return String(this).replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(^|[^\uD800-\uDBFF])[\uDC00-\uDFFF]/g, function (m, p) { return (p || '') + '�'; }); });
  def(Math, 'expm1', function (x) { return Math.exp(x) - 1; });
  def(Math, 'log1p', function (x) { return Math.log(1 + x); });
  var SP = Set.prototype;
  def(SP, 'union', function (o) { var r = new Set(this); for (var x of o.keys ? o.keys() : o) r.add(x); return r; });
  def(SP, 'intersection', function (o) { var r = new Set(); for (var x of this) if (o.has(x)) r.add(x); return r; });
  def(SP, 'difference', function (o) { var r = new Set(); for (var x of this) if (!o.has(x)) r.add(x); return r; });
  def(SP, 'symmetricDifference', function (o) { var r = new Set(this); for (var x of o.keys ? o.keys() : o) { if (this.has(x)) r['delete'](x); else r.add(x); } return r; });
  def(SP, 'isSubsetOf', function (o) { for (var x of this) if (!o.has(x)) return false; return true; });
  def(SP, 'isSupersetOf', function (o) { for (var x of o.keys ? o.keys() : o) if (!this.has(x)) return false; return true; });
  def(SP, 'isDisjointFrom', function (o) { for (var x of this) if (o.has(x)) return false; return true; });
  if (typeof AggregateError === 'undefined') {
    window.AggregateError = function AggregateError(errors, message) {
      var e = new Error(message);
      Object.setPrototypeOf(e, AggregateError.prototype);
      e.errors = Array.from(errors || []);
      return e;
    };
    AggregateError.prototype = Object.create(Error.prototype, { name: { value: 'AggregateError', writable: true, configurable: true }, constructor: { value: AggregateError, writable: true, configurable: true } });
  }
  def(Error, 'captureStackTrace', function (o) { if (o && typeof o === 'object' && !('stack' in o)) o.stack = String(o); });

  // requestIdleCallback: runs soon after, with an IdleDeadline (up to 50ms).
  (function () {
    var timer = setTimeout, clear = clearTimeout;
    window.requestIdleCallback = function (cb, opts) {
      return timer(function () {
        var start = performance.now();
        cb({ didTimeout: false, timeRemaining: function () { return Math.max(0, 50 - (performance.now() - start)); } });
      }, 1);
    };
    window.cancelIdleCallback = function (id) { clear(id); };
  })();

  if (typeof PerformanceObserver !== 'undefined' && !PerformanceObserver.supportedEntryTypes) {
    PerformanceObserver.supportedEntryTypes = ['element', 'event', 'first-input', 'largest-contentful-paint',
      'layout-shift', 'longtask', 'mark', 'measure', 'navigation', 'paint', 'resource'];
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

  // Put each DOM method on the interface the spec defines it on (polyfills
  // such as Shady DOM copy e.g. Element.prototype.setAttribute). The
  // natives are installed on Node.prototype; move them out.
  (function () {
    var NP = Node.prototype;
    var homes = {
      Element: ['getAttribute', 'setAttribute', 'setAttributeNS', 'removeAttribute', 'hasAttribute', 'hasAttributes',
        'getAttributeNode', 'toggleAttribute', 'getAttributeNames', 'closest', 'matches', 'insertAdjacentHTML',
        'getBoundingClientRect', 'scrollIntoView', 'scrollTo', 'scrollBy', 'setPointerCapture', 'releasePointerCapture',
        'attachShadow', 'getElementsByClassName', 'getElementsByTagName'],
      HTMLElement: ['click', 'focus', 'blur'],
      SVGElement: ['focus', 'blur'],
      HTMLCanvasElement: ['getContext', 'toDataURL'],
      HTMLFormElement: ['submit', 'requestSubmit'],
      HTMLInputElement: ['select', 'setSelectionRange'],
      HTMLTextAreaElement: ['select', 'setSelectionRange'],
      HTMLSlotElement: ['assignedNodes', 'assignedElements'],
      HTMLMediaElement: ['load', 'play', 'pause', 'canPlayType', 'addTextTrack', 'fastSeek', 'captureStream'],
      HTMLVideoElement: ['requestPictureInPicture'],
      Document: ['getElementById', 'elementFromPoint', 'elementsFromPoint', 'createElement', 'createElementNS', 'createTextNode', 'createComment',
        'createDocumentFragment', 'write', 'writeln', 'hasFocus', 'importNode', 'adoptNode', 'createRange',
        'getElementsByClassName', 'getElementsByTagName', 'getElementsByName'],
      DocumentFragment: ['getElementById'],
      // ParentNode / ChildNode mixins.
      '+parent': ['append', 'prepend', 'querySelector', 'querySelectorAll'],
      '+child': ['remove', 'before', 'after', 'replaceWith']
    };
    var mixins = { '+parent': ['Element', 'Document', 'DocumentFragment'], '+child': ['Element', 'CharacterData', 'DocumentType'] };
    // Accessors too: `'data' in div` must be false (sanitizers such as
    // polymer_resin and feature tests probe which properties elements have).
    var acc = {
      '+parent': ['children', 'firstElementChild', 'lastElementChild', 'childElementCount'],
      '+elsib': ['nextElementSibling', 'previousElementSibling'],
      Element: ['innerHTML', 'outerHTML', 'id', 'className', 'classList', 'tagName', 'localName', 'namespaceURI',
        'attributes', 'shadowRoot', 'slot', 'clientWidth', 'clientHeight', 'scrollWidth', 'scrollHeight',
        'scrollTop', 'scrollLeft'],
      '+slottable': ['assignedSlot'],
      HTMLElement: ['innerText', 'hidden', 'offsetParent', 'offsetWidth', 'offsetHeight', 'offsetLeft', 'offsetTop',
        'contentEditable', 'isContentEditable'],
      '+htmlsvg': ['style', 'dataset', 'tabIndex'],
      '+titled': ['title', 'lang', 'dir'],
      '+docroot': ['activeElement'],
      '+host': ['host'],
      '+value': ['value'],
      '+disabled': ['disabled'],
      '+href': ['href'],
      '+src': ['src'],
      '+name': ['name'],
      '+type': ['type'],
      '+data': ['data'],
      HTMLMediaElement: ['currentTime', 'paused', 'ended', 'muted', 'volume', 'playbackRate', 'duration',
        'networkState', 'buffered', 'seekable', 'played', 'currentSrc', 'textTracks'],
      '+ready': ['readyState'],
      HTMLVideoElement: ['videoWidth', 'videoHeight'],
      '+frame': ['contentWindow', 'contentDocument'],
      HTMLFormElement: ['action', 'method'],
      '+target': ['target'],
      '+rel': ['rel'],
      '+alt': ['alt'],
      '+textlike': ['placeholder', 'readOnly'],
      HTMLOptionElement: ['selected'],
      '+multiple': ['multiple'],
      '+required': ['required'],
      HTMLInputElement: ['checked', 'min', 'step', 'pattern'],
      '+max': ['max'],
      '+form': ['form'],
      '+label': ['label'],
      '+for': ['htmlFor'],
      '+size': ['width', 'height']
    };
    var forms = ['HTMLInputElement', 'HTMLButtonElement', 'HTMLSelectElement', 'HTMLTextAreaElement'];
    var accMixins = {
      '+parent': mixins['+parent'],
      '+elsib': ['Element', 'CharacterData'],
      '+slottable': ['Element', 'Text'],
      '+htmlsvg': ['HTMLElement', 'SVGElement', 'MathMLElement'],
      '+titled': ['HTMLElement', 'Document'],
      '+docroot': ['Document', 'ShadowRoot'],
      '+host': ['ShadowRoot', 'HTMLAnchorElement', 'HTMLAreaElement'],
      '+value': forms.concat(['HTMLOptionElement', 'HTMLOutputElement', 'HTMLDataElement', 'HTMLLIElement',
        'HTMLMeterElement', 'HTMLProgressElement', 'HTMLParamElement', 'Attr']),
      '+disabled': forms.concat(['HTMLOptionElement', 'HTMLOptGroupElement', 'HTMLFieldSetElement', 'HTMLLinkElement', 'HTMLStyleElement']),
      '+href': ['HTMLAnchorElement', 'HTMLAreaElement', 'HTMLLinkElement', 'HTMLBaseElement'],
      '+src': ['HTMLImageElement', 'HTMLScriptElement', 'HTMLIFrameElement', 'HTMLMediaElement', 'HTMLSourceElement',
        'HTMLEmbedElement', 'HTMLInputElement', 'HTMLTrackElement', 'HTMLFrameElement'],
      '+name': forms.concat(['HTMLFormElement', 'HTMLIFrameElement', 'HTMLMetaElement', 'HTMLAnchorElement', 'HTMLMapElement',
        'HTMLObjectElement', 'HTMLParamElement', 'HTMLOutputElement', 'HTMLFieldSetElement', 'HTMLSlotElement',
        'HTMLEmbedElement', 'HTMLFrameElement', 'HTMLImageElement', 'HTMLDetailsElement', 'DocumentType', 'Attr']),
      '+type': forms.concat(['HTMLScriptElement', 'HTMLStyleElement', 'HTMLLinkElement', 'HTMLSourceElement', 'HTMLObjectElement',
        'HTMLEmbedElement', 'HTMLOListElement', 'HTMLAnchorElement', 'HTMLOutputElement', 'HTMLFieldSetElement', 'HTMLLIElement']),
      '+data': ['CharacterData', 'HTMLObjectElement'],
      '+ready': ['Document', 'HTMLMediaElement'],
      '+frame': ['HTMLIFrameElement', 'HTMLFrameElement', 'HTMLObjectElement'],
      '+target': ['HTMLAnchorElement', 'HTMLAreaElement', 'HTMLFormElement', 'HTMLBaseElement', 'HTMLLinkElement'],
      '+rel': ['HTMLAnchorElement', 'HTMLAreaElement', 'HTMLLinkElement'],
      '+alt': ['HTMLImageElement', 'HTMLInputElement', 'HTMLAreaElement'],
      '+textlike': ['HTMLInputElement', 'HTMLTextAreaElement'],
      '+multiple': ['HTMLInputElement', 'HTMLSelectElement'],
      '+required': ['HTMLInputElement', 'HTMLSelectElement', 'HTMLTextAreaElement'],
      '+max': ['HTMLInputElement', 'HTMLMeterElement', 'HTMLProgressElement'],
      '+form': forms.concat(['HTMLOptionElement', 'HTMLFieldSetElement', 'HTMLOutputElement', 'HTMLObjectElement',
        'HTMLLabelElement', 'HTMLLegendElement']),
      '+label': ['HTMLOptionElement', 'HTMLOptGroupElement', 'HTMLTrackElement'],
      '+for': ['HTMLLabelElement', 'HTMLOutputElement'],
      '+size': ['HTMLImageElement', 'HTMLCanvasElement', 'HTMLVideoElement', 'HTMLIFrameElement', 'HTMLEmbedElement',
        'HTMLObjectElement', 'HTMLInputElement', 'HTMLSourceElement']
    };
    Object.keys(acc).forEach(function (k) {
      homes[k] = (homes[k] || []).concat(acc[k]);
      if (accMixins[k]) mixins[k] = accMixins[k];
    });
    var moved = {};
    Object.keys(homes).forEach(function (iface) {
      var targets = mixins[iface] || [iface];
      homes[iface].forEach(function (name) {
        var d = Object.getOwnPropertyDescriptor(NP, name);
        if (!d) return;
        targets.forEach(function (t) {
          var C = window[t];
          if (C && C.prototype && !Object.prototype.hasOwnProperty.call(C.prototype, name)) Object.defineProperty(C.prototype, name, d);
        });
        moved[name] = true;
      });
    });
    Object.keys(moved).forEach(function (name) { delete NP[name]; });
  })();
})();

// window.name (a frame's is set by the page that loads it).
if (typeof window.name !== 'string') window.name = '';

// Every global constructor's prototype points back at it.
(function () {
  Object.getOwnPropertyNames(window).forEach(function (n) {
    var C;
    try { C = window[n]; } catch (e) { return; }
    if (typeof C !== 'function' || !/^[A-Z]/.test(n)) return;
    var P = C.prototype;
    if (P && typeof P === 'object' && !Object.prototype.hasOwnProperty.call(P, 'constructor')) {
      Object.defineProperty(P, 'constructor', { value: C, writable: true, configurable: true, enumerable: false });
    }
  });
})();

// DOMParser: documents from markup (a detached document in this arena).
(function () {
  if (window.DOMParser) return;
  function DOMParser() {}
  DOMParser.prototype.parseFromString = function (str, type) {
    str = String(str);
    type = String(type || 'text/html').toLowerCase();
    var d = document.implementation.createHTMLDocument('');
    var html = d.documentElement;
    if (type.indexOf('html') >= 0 && type.indexOf('xhtml') < 0) {
      // Whole documents keep their <head>; everything else goes in <body>.
      var m = /<head[^>]*>([\s\S]*?)<\/head>/i.exec(str);
      var b = /<body[^>]*>([\s\S]*?)(<\/body>|$)/i.exec(str);
      if (m) d.head.innerHTML = m[1];
      var bodyHtml = b ? b[1] : str.replace(/<!doctype[^>]*>/i, '').replace(/<\/?html[^>]*>/gi, '').replace(/<head[^>]*>[\s\S]*?<\/head>/i, '');
      d.body.innerHTML = bodyHtml;
      var title = d.querySelector('title');
      return d;
    }
    // XML (SVG, feeds): the markup's elements become the document's children.
    d.removeChild(html);
    var tmp = document.createElement('div');
    tmp.innerHTML = str.replace(/<\?xml[^>]*\?>/, '').replace(/<!doctype[^>]*>/i, '');
    while (tmp.firstChild) d.appendChild(tmp.firstChild);
    return d;
  };
  window.DOMParser = DOMParser;
})();

// XMLSerializer: markup back from nodes.
(function () {
  if (window.XMLSerializer) return;
  function XMLSerializer() {}
  XMLSerializer.prototype.serializeToString = function (n) {
    if (!n) return '';
    if (n.nodeType === 9) { var e = n.documentElement; return e ? e.outerHTML : ''; }
    if (n.nodeType === 1) return n.outerHTML;
    if (n.nodeType === 11) { var s = ''; for (var c = n.firstChild; c; c = c.nextSibling) s += XMLSerializer.prototype.serializeToString(c); return s; }
    if (n.nodeType === 3) return String(n.data).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    if (n.nodeType === 8) return '<!--' + n.data + '-->';
    return '';
  };
  window.XMLSerializer = XMLSerializer;
})();

// WebSocket / EventSource: no socket support yet. Connections fail the way a
// refused connection does (error, then close), so pages fall back instead of
// crashing on a throwing constructor.
(function () {
  function failLater(self, closeCode) {
    setTimeout(function () {
      self.readyState = 3;
      var err = new Event('error');
      self.dispatchEvent(err);
      if (closeCode) {
        var c = new Event('close');
        c.code = closeCode; c.reason = ''; c.wasClean = false;
        self.dispatchEvent(c);
      }
    }, 0);
  }
  // (EventTarget's dispatch also calls the on<type> handlers.)
  var Emitter = EventTarget;
  function WebSocket(url, protocols) {
    if (!(this instanceof WebSocket)) throw new TypeError("Failed to construct 'WebSocket': Please use the 'new' operator");
    EventTarget.call(this);
    this.url = String(url); this.readyState = 0; this.protocol = ''; this.extensions = '';
    this.bufferedAmount = 0; this.binaryType = 'blob';
    this.onopen = this.onmessage = this.onerror = this.onclose = null;
    failLater(this, 1006);
  }
  WebSocket.prototype = Object.create(Emitter.prototype);
  WebSocket.prototype.constructor = WebSocket;
  WebSocket.prototype.send = function () {
    if (this.readyState === 0) throw new DOMException("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.", 'InvalidStateError');
  };
  WebSocket.prototype.close = function () { this.readyState = 3; };
  ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function (k, i) { WebSocket[k] = i; WebSocket.prototype[k] = i; });
  window.WebSocket = WebSocket;

  function EventSource(url, init) {
    if (!(this instanceof EventSource)) throw new TypeError("Failed to construct 'EventSource': Please use the 'new' operator");
    EventTarget.call(this);
    this.url = String(url); this.readyState = 0; this.withCredentials = !!(init && init.withCredentials);
    this.onopen = this.onmessage = this.onerror = null;
    failLater(this, 0);
  }
  EventSource.prototype = Object.create(Emitter.prototype);
  EventSource.prototype.constructor = EventSource;
  EventSource.prototype.close = function () { this.readyState = 2; };
  ['CONNECTING', 'OPEN', 'CLOSED'].forEach(function (k, i) { EventSource[k] = i; EventSource.prototype[k] = i; });
  window.EventSource = EventSource;
})();

// IndexedDB (in memory for the page's lifetime): databases, object stores,
// indexes, cursors, key ranges; requests complete asynchronously and a
// transaction completes once its requests have.
(function () {
  if (window.indexedDB) return;
  var dbs = {};
  var clone = function (v) { try { return structuredClone(v); } catch (e) { return v; } };
  function later(fn) { setTimeout(fn, 0); }
  function domErr(name, msg) { return new DOMException(msg || name, name); }
  function fire(target, type, props, cancelable) {
    var e = new Event(type, { bubbles: false, cancelable: !!cancelable });
    if (props) for (var k in props) e[k] = props[k];
    target.dispatchEvent(e);
    return e;
  }
  function list(arr) {
    var a = arr.slice().sort();
    a.contains = function (x) { return a.indexOf(x) >= 0; };
    a.item = function (i) { return a[i] === undefined ? null : a[i]; };
    return a;
  }

  // Keys: number < Date < string < binary < array.
  function keyType(k) {
    if (typeof k === 'number') return isNaN(k) ? -1 : 1;
    if (k instanceof Date) return isNaN(k.getTime()) ? -1 : 2;
    if (typeof k === 'string') return 3;
    if (k instanceof ArrayBuffer || ArrayBuffer.isView(k)) return 4;
    if (Array.isArray(k)) return 5;
    return -1;
  }
  function validKey(k) {
    var t = keyType(k);
    if (t === 5) return k.every(validKey);
    return t > 0;
  }
  function cmp(a, b) {
    var ta = keyType(a), tb = keyType(b);
    if (ta !== tb) return ta < tb ? -1 : 1;
    if (ta === 2) { a = a.getTime(); b = b.getTime(); }
    if (ta === 4) {
      var x = new Uint8Array(a.buffer || a), y = new Uint8Array(b.buffer || b);
      for (var i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) return x[i] < y[i] ? -1 : 1;
      return x.length === y.length ? 0 : x.length < y.length ? -1 : 1;
    }
    if (ta === 5) {
      for (var j = 0; j < Math.min(a.length, b.length); j++) { var c = cmp(a[j], b[j]); if (c) return c; }
      return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
    }
    return a === b ? 0 : a < b ? -1 : 1;
  }
  function evalPath(v, path) {
    if (Array.isArray(path)) return path.map(function (p) { return evalPath(v, p); });
    if (path === '') return v;
    var parts = String(path).split('.');
    for (var i = 0; i < parts.length; i++) {
      if (v == null || (typeof v !== 'object' && typeof v !== 'string')) return undefined;
      v = v[parts[i]];
    }
    return v;
  }
  function setPath(v, path, key) {
    var parts = String(path).split('.');
    for (var i = 0; i < parts.length - 1; i++) { if (v[parts[i]] == null) v[parts[i]] = {}; v = v[parts[i]]; }
    v[parts[parts.length - 1]] = key;
  }

  function IDBKeyRange(lower, upper, lowerOpen, upperOpen) {
    this.lower = lower; this.upper = upper; this.lowerOpen = !!lowerOpen; this.upperOpen = !!upperOpen;
  }
  IDBKeyRange.prototype.includes = function (k) {
    if (this.lower !== undefined) { var c = cmp(k, this.lower); if (c < 0 || (c === 0 && this.lowerOpen)) return false; }
    if (this.upper !== undefined) { var d = cmp(k, this.upper); if (d > 0 || (d === 0 && this.upperOpen)) return false; }
    return true;
  };
  IDBKeyRange.only = function (k) { if (!validKey(k)) throw domErr('DataError'); return new IDBKeyRange(k, k, false, false); };
  IDBKeyRange.lowerBound = function (k, open) { return new IDBKeyRange(k, undefined, open, true); };
  IDBKeyRange.upperBound = function (k, open) { return new IDBKeyRange(undefined, k, true, open); };
  IDBKeyRange.bound = function (l, u, lo, uo) { if (cmp(l, u) > 0) throw domErr('DataError'); return new IDBKeyRange(l, u, lo, uo); };
  function toRange(q) {
    if (q === undefined || q === null) return null;
    if (q instanceof IDBKeyRange) return q;
    if (!validKey(q)) throw domErr('DataError', 'The parameter is not a valid key.');
    return IDBKeyRange.only(q);
  }
  function inRange(r, k) { return !r || r.includes(k); }

  function IDBRequest(source, tx) {
    EventTarget.call(this);
    this.readyState = 'pending'; this.result = undefined; this.error = null;
    this.source = source || null; this.transaction = tx || null;
    this.onsuccess = this.onerror = null;
  }
  IDBRequest.prototype = Object.create(EventTarget.prototype);
  IDBRequest.prototype.constructor = IDBRequest;
  function IDBOpenDBRequest() { IDBRequest.call(this); this.onupgradeneeded = this.onblocked = null; }
  IDBOpenDBRequest.prototype = Object.create(IDBRequest.prototype);
  IDBOpenDBRequest.prototype.constructor = IDBOpenDBRequest;

  // A request on a transaction: run `op` later; its value is the result.
  function request(tx, source, op) {
    var req = new IDBRequest(source, tx);
    tx._pending++;
    later(function () {
      if (tx._aborted) { tx._pending--; return; }
      var val, err = null;
      try { val = op(); } catch (e) { err = e; }
      req.readyState = 'done';
      if (err) {
        req.error = err;
        var ev = fire(req, 'error', null, true);
        if (!ev.defaultPrevented) { tx._pending--; tx.abort(err); return; }
      } else {
        req.result = val;
        fire(req, 'success');
      }
      tx._pending--;
      tx._maybeFinish();
    });
    return req;
  }

  function IDBTransaction(db, names, mode) {
    EventTarget.call(this);
    this.db = db; this.mode = mode || 'readonly'; this.objectStoreNames = list(names);
    this.error = null; this.oncomplete = this.onerror = this.onabort = null;
    this._pending = 0; this._done = false; this._aborted = false;
    var tx = this;
    later(function () { tx._maybeFinish(); });
  }
  IDBTransaction.prototype = Object.create(EventTarget.prototype);
  IDBTransaction.prototype.constructor = IDBTransaction;
  IDBTransaction.prototype._maybeFinish = function () {
    var tx = this;
    if (tx._done || tx._pending > 0) return;
    later(function () {
      if (tx._done || tx._pending > 0) return;
      tx._done = true;
      fire(tx, 'complete');
    });
  };
  IDBTransaction.prototype.objectStore = function (name) {
    if (this.objectStoreNames.indexOf(name) < 0) throw domErr('NotFoundError', 'No objectStore named ' + name + ' in this transaction');
    return new IDBObjectStore(this, this.db._data.stores[name], name);
  };
  IDBTransaction.prototype.abort = function (err) {
    if (this._done) return;
    this._done = this._aborted = true;
    this.error = err || null;
    var tx = this;
    later(function () { if (err) fire(tx, 'error'); fire(tx, 'abort'); });
  };
  IDBTransaction.prototype.commit = function () {};

  function IDBObjectStore(tx, store, name) {
    this.transaction = tx; this._s = store; this.name = name;
    this.keyPath = store.keyPath; this.autoIncrement = store.autoIncrement;
    this.indexNames = list(Object.keys(store.indexes));
  }
  function findPos(recs, key) {
    var lo = 0, hi = recs.length;
    while (lo < hi) { var m = (lo + hi) >> 1; if (cmp(recs[m].key, key) < 0) lo = m + 1; else hi = m; }
    return lo;
  }
  function writable(tx) {
    if (tx.mode === 'readonly') throw domErr('ReadOnlyError', 'The transaction is read-only.');
    if (tx._done) throw domErr('TransactionInactiveError', 'The transaction has finished.');
  }
  IDBObjectStore.prototype._put = function (value, key, noOverwrite) {
    var s = this._s;
    value = clone(value);
    if (s.keyPath != null) {
      if (key !== undefined) throw domErr('DataError', 'The object store uses in-line keys and the key parameter was provided.');
      key = evalPath(value, s.keyPath);
      if (key === undefined && s.autoIncrement) { key = s.next++; setPath(value, s.keyPath, key); }
    } else if (key === undefined && s.autoIncrement) key = s.next++;
    if (!validKey(key)) throw domErr('DataError', 'No valid key for the record.');
    if (typeof key === 'number' && s.autoIncrement && key >= s.next) s.next = Math.floor(key) + 1;
    var i = findPos(s.records, key);
    var exists = i < s.records.length && cmp(s.records[i].key, key) === 0;
    if (exists && noOverwrite) throw domErr('ConstraintError', 'Key already exists in the object store.');
    for (var n in s.indexes) {
      var ix = s.indexes[n];
      if (!ix.unique) continue;
      var ik = evalPath(value, ix.keyPath);
      if (ik === undefined) continue;
      for (var r = 0; r < s.records.length; r++) {
        if (exists && r === i) continue;
        if (cmp(evalPath(s.records[r].value, ix.keyPath), ik) === 0) throw domErr('ConstraintError', 'Unique index constraint violated.');
      }
    }
    if (exists) s.records[i].value = value; else s.records.splice(i, 0, { key: key, value: value });
    return key;
  };
  IDBObjectStore.prototype.put = function (value, key) { writable(this.transaction); var self = this; return request(this.transaction, this, function () { return self._put(value, key, false); }); };
  IDBObjectStore.prototype.add = function (value, key) { writable(this.transaction); var self = this; return request(this.transaction, this, function () { return self._put(value, key, true); }); };
  IDBObjectStore.prototype._matching = function (q) {
    var r = toRange(q);
    return this._s.records.filter(function (rec) { return inRange(r, rec.key); });
  };
  IDBObjectStore.prototype.get = function (q) { var self = this; return request(this.transaction, this, function () { var m = self._matching(q)[0]; return m ? clone(m.value) : undefined; }); };
  IDBObjectStore.prototype.getKey = function (q) { var self = this; return request(this.transaction, this, function () { var m = self._matching(q)[0]; return m ? m.key : undefined; }); };
  IDBObjectStore.prototype.getAll = function (q, count) { var self = this; return request(this.transaction, this, function () { var m = self._matching(q); if (count) m = m.slice(0, count); return m.map(function (r) { return clone(r.value); }); }); };
  IDBObjectStore.prototype.getAllKeys = function (q, count) { var self = this; return request(this.transaction, this, function () { var m = self._matching(q); if (count) m = m.slice(0, count); return m.map(function (r) { return r.key; }); }); };
  IDBObjectStore.prototype.count = function (q) { var self = this; return request(this.transaction, this, function () { return self._matching(q).length; }); };
  IDBObjectStore.prototype.delete = function (q) {
    writable(this.transaction); var self = this;
    return request(this.transaction, this, function () { var r = toRange(q); self._s.records = self._s.records.filter(function (rec) { return !inRange(r, rec.key); }); return undefined; });
  };
  IDBObjectStore.prototype.clear = function () { writable(this.transaction); var self = this; return request(this.transaction, this, function () { self._s.records = []; return undefined; }); };
  IDBObjectStore.prototype.createIndex = function (name, keyPath, opts) {
    if (this.transaction.mode !== 'versionchange') throw domErr('InvalidStateError');
    if (this._s.indexes[name]) throw domErr('ConstraintError', 'Index already exists');
    this._s.indexes[name] = { keyPath: keyPath, unique: !!(opts && opts.unique), multiEntry: !!(opts && opts.multiEntry) };
    this.indexNames = list(Object.keys(this._s.indexes));
    return this.index(name);
  };
  IDBObjectStore.prototype.deleteIndex = function (name) { delete this._s.indexes[name]; this.indexNames = list(Object.keys(this._s.indexes)); };
  IDBObjectStore.prototype.index = function (name) {
    var ix = this._s.indexes[name];
    if (!ix) throw domErr('NotFoundError', 'No index named ' + name);
    return new IDBIndex(this, ix, name);
  };
  function storeEntries(store, q) { return store._matching(q).map(function (r) { return { key: r.key, primaryKey: r.key, rec: r }; }); }
  IDBObjectStore.prototype.openCursor = function (q, dir) { return openCursor(this, storeEntries(this, q), dir, true); };
  IDBObjectStore.prototype.openKeyCursor = function (q, dir) { return openCursor(this, storeEntries(this, q), dir, false); };

  function IDBIndex(store, ix, name) {
    this.objectStore = store; this._ix = ix; this.name = name;
    this.keyPath = ix.keyPath; this.unique = ix.unique; this.multiEntry = ix.multiEntry;
  }
  IDBIndex.prototype._entries = function (q) {
    var r = toRange(q), ix = this._ix, out = [];
    this.objectStore._s.records.forEach(function (rec) {
      var k = evalPath(rec.value, ix.keyPath);
      var ks = ix.multiEntry && Array.isArray(k) ? k : [k];
      ks.forEach(function (key) { if (validKey(key) && inRange(r, key)) out.push({ key: key, primaryKey: rec.key, rec: rec }); });
    });
    out.sort(function (a, b) { return cmp(a.key, b.key) || cmp(a.primaryKey, b.primaryKey); });
    return out;
  };
  IDBIndex.prototype.get = function (q) { var self = this; return request(this.objectStore.transaction, this, function () { var e = self._entries(q)[0]; return e ? clone(e.rec.value) : undefined; }); };
  IDBIndex.prototype.getKey = function (q) { var self = this; return request(this.objectStore.transaction, this, function () { var e = self._entries(q)[0]; return e ? e.primaryKey : undefined; }); };
  IDBIndex.prototype.getAll = function (q, count) { var self = this; return request(this.objectStore.transaction, this, function () { var e = self._entries(q); if (count) e = e.slice(0, count); return e.map(function (x) { return clone(x.rec.value); }); }); };
  IDBIndex.prototype.getAllKeys = function (q, count) { var self = this; return request(this.objectStore.transaction, this, function () { var e = self._entries(q); if (count) e = e.slice(0, count); return e.map(function (x) { return x.primaryKey; }); }); };
  IDBIndex.prototype.count = function (q) { var self = this; return request(this.objectStore.transaction, this, function () { return self._entries(q).length; }); };
  IDBIndex.prototype.openCursor = function (q, dir) { return openCursor(this, this._entries(q), dir, true); };
  IDBIndex.prototype.openKeyCursor = function (q, dir) { return openCursor(this, this._entries(q), dir, false); };

  function IDBCursor(source, req, entries, dir, withValue) {
    this.source = source; this.request = req; this.direction = dir || 'next';
    this._e = entries; this._i = -1; this._withValue = withValue;
  }
  IDBCursor.prototype._step = function (n) {
    var c = this, tx = this.request.transaction;
    tx._pending++;
    later(function () {
      c._i += n;
      var e = c._e[c._i];
      if (e) {
        c.key = e.key; c.primaryKey = e.primaryKey;
        if (c._withValue) c.value = clone(e.rec.value);
        c.request.result = c;
      } else c.request.result = null;
      c.request.readyState = 'done';
      fire(c.request, 'success');
      tx._pending--;
      tx._maybeFinish();
    });
  };
  IDBCursor.prototype.continue = function (key) {
    if (key !== undefined) {
      var i = this._i + 1, up = this.direction.indexOf('prev') < 0;
      while (i < this._e.length && (up ? cmp(this._e[i].key, key) < 0 : cmp(this._e[i].key, key) > 0)) i++;
      this._step(i - this._i);
    } else this._step(1);
  };
  IDBCursor.prototype.advance = function (n) { this._step(n); };
  IDBCursor.prototype.update = function (v) { var rec = this._e[this._i].rec; return request(this.request.transaction, this, function () { rec.value = clone(v); return rec.key; }); };
  IDBCursor.prototype.delete = function () { var store = this.source.objectStore || this.source; return store.delete(this._e[this._i].primaryKey); };
  function openCursor(source, entries, dir, withValue) {
    var store = source.objectStore || source;
    if (dir && dir.indexOf('prev') === 0) entries = entries.slice().reverse();
    if (dir && dir.indexOf('unique') > 0) entries = entries.filter(function (e, i) { return i === 0 || cmp(e.key, entries[i - 1].key) !== 0; });
    var req = new IDBRequest(source, store.transaction);
    var c = new IDBCursor(source, req, entries, dir, withValue);
    c._step(1);
    return req;
  }

  function IDBDatabase(name, data) {
    EventTarget.call(this);
    this.name = name; this._data = data; this.version = data.version;
    this.objectStoreNames = list(Object.keys(data.stores));
    this.onversionchange = this.onclose = this.onabort = this.onerror = null;
    this._upgradeTx = null;
  }
  IDBDatabase.prototype = Object.create(EventTarget.prototype);
  IDBDatabase.prototype.constructor = IDBDatabase;
  IDBDatabase.prototype.createObjectStore = function (name, opts) {
    if (!this._upgradeTx) throw domErr('InvalidStateError', 'The database is not running a version change transaction.');
    if (this._data.stores[name]) throw domErr('ConstraintError', 'An object store with the specified name already exists.');
    this._data.stores[name] = { keyPath: opts && opts.keyPath != null ? opts.keyPath : null, autoIncrement: !!(opts && opts.autoIncrement), next: 1, records: [], indexes: {} };
    this.objectStoreNames = list(Object.keys(this._data.stores));
    this._upgradeTx.objectStoreNames = this.objectStoreNames;
    return this._upgradeTx.objectStore(name);
  };
  IDBDatabase.prototype.deleteObjectStore = function (name) {
    delete this._data.stores[name];
    this.objectStoreNames = list(Object.keys(this._data.stores));
  };
  IDBDatabase.prototype.transaction = function (names, mode) {
    names = typeof names === 'string' ? [names] : Array.prototype.slice.call(names);
    for (var i = 0; i < names.length; i++) if (!this._data.stores[names[i]]) throw domErr('NotFoundError', 'One of the specified object stores was not found.');
    return new IDBTransaction(this, names, mode);
  };
  IDBDatabase.prototype.close = function () {};

  var factory = {
    open: function (name, version) {
      if (version !== undefined && (!(version >= 1) || version !== Math.floor(version))) throw new TypeError('The version provided must be a positive integer.');
      var req = new IDBOpenDBRequest();
      later(function () {
        var data = dbs[name] || (dbs[name] = { version: 0, stores: {} });
        var old = data.version, nv = version === undefined ? Math.max(old, 1) : version;
        if (nv < old) { req.readyState = 'done'; req.error = domErr('VersionError', 'The requested version is less than the existing version.'); fire(req, 'error'); return; }
        var db = new IDBDatabase(name, data);
        req.result = db;
        if (nv > old) {
          data.version = nv; db.version = nv;
          var tx = new IDBTransaction(db, Object.keys(data.stores), 'versionchange');
          db._upgradeTx = tx; req.transaction = tx; req.readyState = 'done';
          fire(req, 'upgradeneeded', { oldVersion: old, newVersion: nv });
          tx.addEventListener('complete', function () {
            db._upgradeTx = null; req.transaction = null;
            db.objectStoreNames = list(Object.keys(data.stores));
            fire(req, 'success');
          });
          tx.addEventListener('abort', function () { data.version = old; req.error = domErr('AbortError'); fire(req, 'error'); });
        } else {
          req.readyState = 'done';
          fire(req, 'success');
        }
      });
      return req;
    },
    deleteDatabase: function (name) {
      var req = new IDBOpenDBRequest();
      later(function () { var old = dbs[name] ? dbs[name].version : 0; delete dbs[name]; req.readyState = 'done'; fire(req, 'success', { oldVersion: old, newVersion: null }); });
      return req;
    },
    databases: function () { return Promise.resolve(Object.keys(dbs).map(function (n) { return { name: n, version: dbs[n].version }; })); },
    cmp: function (a, b) { return cmp(a, b); }
  };
  window.indexedDB = factory;
  window.IDBKeyRange = IDBKeyRange;
  window.IDBRequest = IDBRequest;
  window.IDBOpenDBRequest = IDBOpenDBRequest;
  window.IDBTransaction = IDBTransaction;
  window.IDBObjectStore = IDBObjectStore;
  window.IDBIndex = IDBIndex;
  window.IDBCursor = IDBCursor;
  window.IDBCursorWithValue = IDBCursor;
  window.IDBDatabase = IDBDatabase;
})();

// Worker / SharedWorker: no worker threads yet. A worker fails to start the
// way an unloadable script does (an error event), instead of the constructor
// throwing.
(function () {
  function makeWorker(name) {
    function W(url, opts) {
      if (!(this instanceof W)) throw new TypeError("Failed to construct '" + name + "': Please use the 'new' operator");
      EventTarget.call(this);
      this.onmessage = this.onerror = this.onmessageerror = null;
      var self = this;
      setTimeout(function () {
        var e = new Event('error');
        e.message = name + ' scripts are not supported';
        self.dispatchEvent(e);
      }, 0);
      if (name === 'SharedWorker') this.port = { postMessage: function () {}, start: function () {}, close: function () {}, addEventListener: function () {}, removeEventListener: function () {}, onmessage: null };
    }
    W.prototype = Object.create(EventTarget.prototype);
    W.prototype.constructor = W;
    W.prototype.postMessage = function () {};
    W.prototype.terminate = function () {};
    Object.defineProperty(W, 'name', { value: name });
    return W;
  }
  window.Worker = makeWorker('Worker');
  window.SharedWorker = makeWorker('SharedWorker');
})();

// postMessage / MessageChannel across windows (iframes) and within one:
// data is structured-serialized, ports have global ids (see messaging.rs).
(function () {
  var portNew = window['@@portNew'], portSend = window['@@portSend'], frameSend = window['@@frameSend'];
  var TA = ['Int8Array', 'Uint8Array', 'Uint8ClampedArray', 'Int16Array', 'Uint16Array', 'Int32Array', 'Uint32Array', 'Float32Array', 'Float64Array'];
  function enc(v, seen) {
    if (v === undefined) return { $u: 1 };
    if (v === null || typeof v === 'boolean' || typeof v === 'string') return v;
    if (typeof v === 'number') return isFinite(v) ? v : { $n: String(v) };
    if (typeof v === 'bigint') return { $b: String(v) };
    if (typeof v === 'function' || typeof v === 'symbol') throw new DOMException('could not be cloned', 'DataCloneError');
    if (v instanceof MessagePort) return { $p: v._id };
    seen = seen || [];
    if (seen.indexOf(v) >= 0) return null;
    seen.push(v);
    if (v instanceof Date) return { $d: v.getTime() };
    if (v instanceof RegExp) return { $r: [v.source, v.flags] };
    if (v instanceof ArrayBuffer) return { $ab: Array.prototype.slice.call(new Uint8Array(v)) };
    for (var i = 0; i < TA.length; i++) if (window[TA[i]] && v instanceof window[TA[i]]) return { $ta: TA[i], v: Array.prototype.slice.call(v) };
    if (v instanceof Map) { var m = []; v.forEach(function (x, k) { m.push([enc(k, seen), enc(x, seen)]); }); return { $m: m }; }
    if (v instanceof Set) { var st = []; v.forEach(function (x) { st.push(enc(x, seen)); }); return { $s: st }; }
    if (Array.isArray(v)) return v.map(function (x) { return enc(x, seen); });
    if (v instanceof Error) return { $e: [v.name, v.message] };
    var o = {};
    Object.keys(v).forEach(function (k) { o[k] = enc(v[k], seen); });
    return { $o: o };
  }
  function dec(v, ports) {
    if (v === null || typeof v !== 'object') return v;
    if (Array.isArray(v)) return v.map(function (x) { return dec(x, ports); });
    if ('$u' in v) return undefined;
    if ('$n' in v) return Number(v.$n);
    if ('$b' in v) return BigInt(v.$b);
    if ('$p' in v) return ports[v.$p] || null;
    if ('$d' in v) return new Date(v.$d);
    if ('$r' in v) return new RegExp(v.$r[0], v.$r[1]);
    if ('$ab' in v) return new Uint8Array(v.$ab).buffer;
    if ('$ta' in v) return new window[v.$ta](v.v);
    if ('$m' in v) return new Map(v.$m.map(function (e) { return [dec(e[0], ports), dec(e[1], ports)]; }));
    if ('$s' in v) return new Set(v.$s.map(function (e) { return dec(e, ports); }));
    if ('$e' in v) { var E = window[v.$e[0]] || Error; return new E(v.$e[1]); }
    var o = {};
    var src = v.$o || {};
    Object.keys(src).forEach(function (k) { o[k] = dec(src[k], ports); });
    return o;
  }
  // [[id, peer], ...] of the ports in a transfer list (they move away).
  function transferList(t) {
    var out = [];
    if (!t) return out;
    Array.prototype.forEach.call(t, function (p) {
      if (p instanceof MessagePort && p._id) { out.push([p._id, p._peer]); delete registry[p._id]; p._closed = true; }
    });
    return out;
  }
  var registry = {};
  function adoptPorts(pairs) {
    var byId = {}, list = [];
    for (var i = 0; i < pairs.length; i++) {
      var p = registry[pairs[i][0]] || new MessagePort(pairs[i][0], pairs[i][1]);
      byId[pairs[i][0]] = p;
      list.push(p);
    }
    return { byId: byId, list: list };
  }

  function MessagePort(id, peer) {
    EventTarget.call(this);
    this._id = id || 0; this._peer = peer || 0;
    this._queue = []; this._started = false; this._closed = false;
    this._onmessage = null;
    if (this._id) registry[this._id] = this;
  }
  MessagePort.prototype = Object.create(EventTarget.prototype);
  MessagePort.prototype.constructor = MessagePort;
  // Setting onmessage starts the port (as in browsers).
  Object.defineProperty(MessagePort.prototype, 'onmessage', {
    get: function () { return this._onmessage; },
    set: function (f) { this._onmessage = f; this.start(); },
    configurable: true
  });
  MessagePort.prototype.postMessage = function (data, transfer) {
    if (this._closed || !this._peer) return;
    if (transfer && !Array.isArray(transfer) && transfer.transfer) transfer = transfer.transfer;
    var json = JSON.stringify(enc(data));
    portSend(this._peer, json, transferList(transfer));
  };
  MessagePort.prototype.start = function () {
    if (this._started) return;
    this._started = true;
    var q = this._queue; this._queue = [];
    for (var i = 0; i < q.length; i++) this._dispatch(q[i]);
  };
  MessagePort.prototype.close = function () { this._closed = true; delete registry[this._id]; };
  // (EventTarget's dispatch calls onmessage too.)
  MessagePort.prototype._dispatch = function (ev) { this.dispatchEvent(ev); };
  var addL = EventTarget.prototype.addEventListener;
  MessagePort.prototype.addEventListener = function (t, f, o) { return addL.call(this, t, f, o); };
  function MessageChannel() {
    var a = portNew(), b = portNew();
    this.port1 = new MessagePort(a, b);
    this.port2 = new MessagePort(b, a);
  }
  function makeEvent(data, origin, source, ports) {
    var ev = new MessageEvent('message');
    try {
      Object.defineProperty(ev, 'data', { value: data, configurable: true });
      Object.defineProperty(ev, 'origin', { value: origin || '', configurable: true });
      Object.defineProperty(ev, 'source', { value: source || null, configurable: true });
      Object.defineProperty(ev, 'ports', { value: ports || [], configurable: true });
      Object.defineProperty(ev, 'lastEventId', { value: '', configurable: true });
    } catch (e) { ev.data = data; ev.origin = origin; ev.source = source; ev.ports = ports || []; }
    return ev;
  }
  window['@@portDeliver'] = function (id, json, pairs) {
    var port = registry[id];
    if (!port || port._closed) return;
    var adopted = adoptPorts(pairs);
    var ev = makeEvent(dec(JSON.parse(json), adopted.byId), '', null, adopted.list);
    if (port._started) port._dispatch(ev); else port._queue.push(ev);
  };
  // A message from another frame's window; `sourceEl` is the iframe it came
  // from (null: from the parent window).
  window['@@windowDeliver'] = function (json, pairs, origin, sourceEl) {
    var adopted = adoptPorts(pairs);
    var source = sourceEl ? sourceEl.contentWindow : window.parent;
    window.dispatchEvent(makeEvent(dec(JSON.parse(json), adopted.byId), origin, source, adopted.list));
  };
  // Messages to this same window.
  window.postMessage = function (data, targetOrigin, transfer) {
    if (targetOrigin && typeof targetOrigin === 'object') { transfer = targetOrigin.transfer; }
    var json = JSON.stringify(enc(data));
    var pairs = transferList(transfer);
    setTimeout(function () {
      var adopted = adoptPorts(pairs);
      window.dispatchEvent(makeEvent(dec(JSON.parse(json), adopted.byId), location.origin, window, adopted.list));
    }, 0);
  };
  // Posting into an iframe's window.
  window['@@framePost'] = function (el, data, targetOrigin, transfer) {
    if (targetOrigin && typeof targetOrigin === 'object') { transfer = targetOrigin.transfer; targetOrigin = targetOrigin.targetOrigin; }
    frameSend('frame', el, JSON.stringify(enc(data)), transferList(transfer), targetOrigin === undefined ? '/' : String(targetOrigin));
  };
  // Inside an iframe: window.parent / window.top post to the containing page.
  window['@@makeParentProxy'] = function (parentUrl) {
    var origin = '';
    try { origin = new URL(parentUrl).origin; } catch (e) {}
    var p = {
      postMessage: function (data, targetOrigin, transfer) {
        if (targetOrigin && typeof targetOrigin === 'object') { transfer = targetOrigin.transfer; targetOrigin = targetOrigin.targetOrigin; }
        frameSend('parent', null, JSON.stringify(enc(data)), transferList(transfer), targetOrigin === undefined ? '/' : String(targetOrigin));
      },
      addEventListener: function () {}, removeEventListener: function () {},
      focus: function () {}, blur: function () {}, close: function () {}, closed: false,
      length: 1, opener: null
    };
    // parent.frames[name]: the page's other frames (messages only).
    var siblings = {};
    p.frames = new Proxy({}, {
      get: function (t, k) {
        if (typeof k !== 'string' || k === 'then' || k === 'toJSON') return undefined;
        if (k === 'length') return 0;
        return siblings[k] || (siblings[k] = {
          postMessage: function (data, targetOrigin, transfer) {
            if (targetOrigin && typeof targetOrigin === 'object') { transfer = targetOrigin.transfer; targetOrigin = targetOrigin.targetOrigin; }
            frameSend('sibling', k, JSON.stringify(enc(data)), transferList(transfer), targetOrigin === undefined ? '/' : String(targetOrigin));
          },
          closed: false
        });
      }
    });
    p.window = p; p.self = p; p.parent = p; p.top = p;
    Object.defineProperty(p, 'location', { get: function () { throw new DOMException('Blocked a frame from accessing a cross-origin frame.', 'SecurityError'); } });
    return p;
  };
  window.MessagePort = MessagePort;
  window.MessageChannel = MessageChannel;
})();

// WebAssembly (natives in src/js/wasm/api.rs).
(function () {
  var W = window['@@wasm'];
  function hidden(o, k, v) { Object.defineProperty(o, k, { value: v, enumerable: false, configurable: true, writable: true }); }
  function makeErr(name) {
    var C = function (message) {
      var e = new Error(message);
      Object.setPrototypeOf(e, C.prototype);
      return e;
    };
    C.prototype = Object.create(Error.prototype);
    hidden(C.prototype, 'constructor', C);
    hidden(C.prototype, 'name', name);
    Object.defineProperty(C, 'name', { value: name });
    return C;
  }
  var CompileError = makeErr('CompileError'), LinkError = makeErr('LinkError'), RuntimeError = makeErr('RuntimeError');
  hidden(window, '@@wasmError', function (kind, msg) { return kind === 'LinkError' ? new LinkError(msg) : kind === 'CompileError' ? new CompileError(msg) : new RuntimeError(msg); });
  function bytesOf(src) {
    if (src instanceof ArrayBuffer) return new Uint8Array(src);
    if (ArrayBuffer.isView(src)) return new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
    throw new TypeError('WebAssembly: Argument 0 must be a buffer source');
  }
  function H(o, C, what) {
    if (!(o instanceof C) || !o['@@h']) throw new TypeError('Receiver is not a ' + what);
    return o['@@h'];
  }
  function wrap(h, C) {
    if (h == null) return h;
    if (h['@@w']) return h['@@w'];
    var o = Object.create(C.prototype);
    hidden(o, '@@h', h);
    hidden(h, '@@w', o);
    return o;
  }

  function Module(bytes) {
    if (!(this instanceof Module)) throw new TypeError("WebAssembly.Module must be invoked with 'new'");
    var h = W.compile(bytesOf(bytes));
    if (typeof h === 'string') throw new CompileError(h);
    hidden(this, '@@h', h);
  }
  Module.exports = function (m) { return W.exports(H(m, Module, 'WebAssembly.Module')).map(function (e) { return { name: e[0], kind: e[1] }; }); };
  Module.imports = function (m) { return W.imports(H(m, Module, 'WebAssembly.Module')).map(function (e) { return { module: e[0], name: e[1], kind: e[2] }; }); };
  Module.customSections = function (m, name) { return W.customSections(H(m, Module, 'WebAssembly.Module'), String(name)); };
  Object.defineProperty(Module.prototype, Symbol.toStringTag, { value: 'WebAssembly.Module' });

  function Memory(desc) {
    if (!(this instanceof Memory)) throw new TypeError("WebAssembly.Memory must be invoked with 'new'");
    if (!desc || typeof desc !== 'object') throw new TypeError('WebAssembly.Memory(): Argument 0 must be a memory descriptor');
    if (desc.initial === undefined && desc.minimum === undefined) throw new TypeError("WebAssembly.Memory(): Property 'initial' is required");
    var h = W.memNew(desc.initial !== undefined ? desc.initial : desc.minimum, desc.maximum);
    hidden(this, '@@h', h);
    hidden(h, '@@w', this);
  }
  Object.defineProperty(Memory.prototype, 'buffer', { get: function () { return W.memBuffer(H(this, Memory, 'WebAssembly.Memory')); }, configurable: true });
  hidden(Memory.prototype, 'grow', function (delta) { return W.memGrow(H(this, Memory, 'WebAssembly.Memory'), delta); });
  Object.defineProperty(Memory.prototype, Symbol.toStringTag, { value: 'WebAssembly.Memory' });

  function Table(desc, value) {
    if (!(this instanceof Table)) throw new TypeError("WebAssembly.Table must be invoked with 'new'");
    if (!desc || typeof desc !== 'object') throw new TypeError('WebAssembly.Table(): Argument 0 must be a table descriptor');
    var h = W.tableNew(desc.element, desc.initial !== undefined ? desc.initial : desc.minimum, desc.maximum, value);
    hidden(this, '@@h', h);
    hidden(h, '@@w', this);
  }
  Object.defineProperty(Table.prototype, 'length', { get: function () { return W.tableLen(H(this, Table, 'WebAssembly.Table')); }, configurable: true });
  hidden(Table.prototype, 'get', function (i) { return W.tableGet(H(this, Table, 'WebAssembly.Table'), i); });
  hidden(Table.prototype, 'set', function (i, v) { W.tableSet(H(this, Table, 'WebAssembly.Table'), i, v); });
  hidden(Table.prototype, 'grow', function (n, v) { return W.tableGrow(H(this, Table, 'WebAssembly.Table'), n, v); });
  Object.defineProperty(Table.prototype, Symbol.toStringTag, { value: 'WebAssembly.Table' });

  function Global(desc, v) {
    if (!(this instanceof Global)) throw new TypeError("WebAssembly.Global must be invoked with 'new'");
    if (!desc || typeof desc !== 'object') throw new TypeError('WebAssembly.Global(): Argument 0 must be a global descriptor');
    var h = W.globalNew(desc.value, !!desc.mutable, v);
    hidden(this, '@@h', h);
    hidden(h, '@@w', this);
  }
  Object.defineProperty(Global.prototype, 'value', {
    get: function () { return W.globalGet(H(this, Global, 'WebAssembly.Global')); },
    set: function (v) { W.globalSet(H(this, Global, 'WebAssembly.Global'), v); },
    configurable: true
  });
  hidden(Global.prototype, 'valueOf', function () { return W.globalGet(H(this, Global, 'WebAssembly.Global')); });
  Object.defineProperty(Global.prototype, Symbol.toStringTag, { value: 'WebAssembly.Global' });

  var classes = { memory: Memory, table: Table, global: Global };
  function Instance(module, importObject) {
    if (!(this instanceof Instance)) throw new TypeError("WebAssembly.Instance must be invoked with 'new'");
    var h = H(module, Module, 'WebAssembly.Module');
    var imps = W.imports(h);
    if (imps.length && (importObject === undefined || importObject === null || typeof importObject !== 'object' && typeof importObject !== 'function'))
      throw new TypeError('WebAssembly.Instance(): Imports argument must be present and must be an object');
    var vals = imps.map(function (imp, i) {
      var ns = importObject[imp[0]];
      if (ns === undefined || ns === null || typeof ns !== 'object' && typeof ns !== 'function')
        throw new TypeError('WebAssembly.Instance(): Import #' + i + ' "' + imp[0] + '": module is not an object or function');
      var v = ns[imp[1]];
      if (v && typeof v === 'object' && v['@@h']) return v['@@h'];
      return v;
    });
    var r = W.instantiate(h, vals);
    if (typeof r === 'string') {
      if (r.indexOf('LinkError:') === 0) throw new LinkError(r.slice(10));
      throw new RuntimeError(r);
    }
    var ex = Object.create(null);
    r.forEach(function (e) {
      ex[e[0]] = e[1] === 'function' || e[1] === 'tag' ? e[2] : wrap(e[2], classes[e[1]]);
    });
    Object.freeze(ex);
    Object.defineProperty(this, 'exports', { value: ex, enumerable: true, configurable: true });
  }
  Object.defineProperty(Instance.prototype, Symbol.toStringTag, { value: 'WebAssembly.Instance' });

  function later(f) {
    return new Promise(function (resolve, reject) {
      setTimeout(function () { try { resolve(f()); } catch (e) { reject(e); } }, 0);
    });
  }
  function compile(bytes) {
    var b;
    try { b = new Uint8Array(bytesOf(bytes)); } catch (e) { return Promise.reject(e); }
    return later(function () { return new Module(b); });
  }
  function instantiate(src, importObject) {
    if (src instanceof Module) return later(function () { return new Instance(src, importObject); });
    return compile(src).then(function (module) {
      return { module: module, instance: new Instance(module, importObject) };
    });
  }
  function fromResponse(source) {
    return Promise.resolve(source).then(function (r) {
      if (!r || typeof r.arrayBuffer !== 'function') throw new TypeError('WebAssembly: Argument 0 must be a Response or a Promise of a Response');
      if (r.ok === false) throw new TypeError('WebAssembly: HTTP status code is not ok');
      return r.arrayBuffer();
    });
  }
  function Tag() { throw new TypeError('WebAssembly.Tag is not supported'); }
  function Exception() { throw new TypeError('WebAssembly.Exception is not supported'); }

  var WA = {
    Module: Module, Instance: Instance, Memory: Memory, Table: Table, Global: Global,
    CompileError: CompileError, LinkError: LinkError, RuntimeError: RuntimeError, Tag: Tag, Exception: Exception,
    validate: function (bytes) { return W.validate(bytesOf(bytes)); },
    compile: compile,
    instantiate: instantiate,
    compileStreaming: function (source) { return fromResponse(source).then(compile); },
    instantiateStreaming: function (source, importObject) { return fromResponse(source).then(function (b) { return instantiate(b, importObject); }); }
  };
  Object.keys(WA).forEach(function (k) { Object.defineProperty(WA, k, { enumerable: false }); });
  Object.defineProperty(WA, Symbol.toStringTag, { value: 'WebAssembly' });
  Object.defineProperty(window, 'WebAssembly', { value: WA, writable: true, configurable: true, enumerable: false });
})();
