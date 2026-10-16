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
})();
