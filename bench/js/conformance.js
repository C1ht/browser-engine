// Differential test: output must match between BOLT_VM=0, BOLT_VM=1 and node.
var out = [];
function log() { out.push(Array.prototype.slice.call(arguments).map(function (x) { return typeof x === 'object' ? JSON.stringify(x) : String(x); }).join(' ')); }

// closures and per-iteration bindings
(function () {
  var fs = [];
  for (let i = 0; i < 3; i++) fs.push(() => i);
  log('let-loop', fs.map(f => f()).join());
  var gs = [];
  for (var j = 0; j < 3; j++) gs.push(() => j);
  log('var-loop', gs.map(f => f()).join());
  var hs = [];
  for (const k of [7, 8, 9]) hs.push(() => k);
  log('of-loop', hs.map(f => f()).join());
  function counter() { let c = 0; return { inc: () => ++c, get: () => c }; }
  const c = counter(); c.inc(); c.inc();
  log('counter', c.get());
})();

// hoisting
(function () {
  log('hoist', typeof f, typeof v, v);
  var v = 1;
  function f() { return g(); }
  function g() { return 'g'; }
  log('hoist2', f());
  { function inner() { return 'in'; } log('block-fn', inner()); }
})();

// arguments, rest, defaults, destructuring
(function () {
  function a(x, y) { return arguments.length + ':' + x + ':' + y; }
  log('args', a(1), a(1, 2, 3));
  function r(x, ...rest) { return x + '|' + rest.join(','); }
  log('rest', r(1, 2, 3), r());
  function d(x = 5, { y = 2, z } = {}, [p, q = 'q'] = []) { return [x, y, z, p, q].join(); }
  log('defaults', d(), d(1, { z: 3 }, ['P']));
  const { m, n: { o } = { o: 'deep' }, ...others } = { m: 1, k: 2, l: 3 };
  log('obj-destr', m, o, JSON.stringify(others));
  const [first, , third, ...tail] = 'abcdef';
  log('arr-destr', first, third, tail.join(''));
  let sa, sb; [sa, sb] = [1, 2]; [sa, sb] = [sb, sa];
  log('swap', sa, sb);
  const o2 = {}; ({ a: o2.x, b: o2['y'] } = { a: 10, b: 20 });
  log('assign-destr', o2.x, o2.y);
})();

// control flow
(function () {
  let s = '';
  outer: for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 4; j++) {
      if (j === 2) continue outer;
      if (i === 3) break outer;
      s += i + '' + j + ' ';
    }
  }
  log('labels', s);
  function sw(x) {
    switch (x) {
      case 1: return 'one';
      case 2:
      case 3: { let t = 'two-three'; return t; }
      default: return 'other';
      case 4: return 'four';
    }
  }
  log('switch', sw(1), sw(3), sw(4), sw(9));
  let fall = '';
  switch (2) { case 1: fall += 'a'; case 2: fall += 'b'; case 3: fall += 'c'; break; case 4: fall += 'd'; }
  log('fall', fall);
  let k = 0; do { k++; } while (k < 5);
  let w = 0; while (true) { if (++w > 3) break; }
  log('loops', k, w);
  blk: { log('in-block'); break blk; }
  for (const key in { a: 1, b: 2 }) log('forin', key);
})();

// try / catch / finally
(function () {
  function t1() { try { return 'try'; } finally { log('finally-runs'); } }
  log('t1', t1());
  function t2() { try { throw new Error('boom'); } catch (e) { return 'caught ' + e.message; } finally { log('f2'); } }
  log('t2', t2());
  function t3() { for (let i = 0; i < 3; i++) { try { if (i === 1) continue; if (i === 2) break; } finally { log('f3', i); } } return 'done'; }
  log('t3', t3());
  function t4() { try { try { throw 1; } finally { log('inner-fin'); } } catch (e) { return 'outer ' + e; } }
  log('t4', t4());
  function t5() { try { return 1; } finally { return 2; } }
  log('t5', t5());
  try { null.x; } catch ({ name, message }) { log('destr-catch', name); }
  try { undefinedVar; } catch (e) { log('ref', e.name); }
  function t6() { let r = []; for (const x of [1, 2, 3]) { try { if (x === 2) throw x; r.push(x); } catch (e) { r.push('e' + e); } } return r.join(); }
  log('t6', t6());
})();

// operators
(function () {
  let x = 5;
  x += 2; x -= 1; x *= 3; x /= 2; x %= 5; x **= 2;
  log('compound', x);
  let y = null; y ??= 'def'; let z = 0; z ||= 9; let q = 1; q &&= 7;
  log('logical-assign', y, z, q);
  log('ops', 1 + '2', '3' * '4', 7 >>> 1, -7 >> 1, 5 & 3, 5 | 3, 5 ^ 3, ~5, 2 ** 10, 1 << 31);
  log('cmp', 1 < 2, 'a' < 'b', null == undefined, null === undefined, NaN === NaN, '1' == 1);
  let i = 0; const a = [i++, i++, ++i, i--, --i];
  log('inc', a.join(), i);
  const obj = { n: 1 }; obj.n++; ++obj['n']; obj.n += 10;
  log('member-inc', obj.n);
  log('typeof', typeof undeclared, typeof null, typeof (() => 1), typeof 1n, typeof Symbol());
  log('in', 'x' in { x: 1 }, 'y' in { x: 1 });
  log('delete', (function () { const o = { a: 1, b: 2 }; delete o.a; delete o['b']; return Object.keys(o).length; })());
  log('void', void 0, void 'x');
  log('cond', true ? 'a' : 'b', 0 ? 'a' : 'b');
  log('seq', (1, 2, 3));
  log('bigint', 2n ** 64n, 10n / 3n, -(5n));
  let s = 'a'; s += 'b'; s = s + 1 + 2;
  log('strcat', s, `t${1 + 1}x${s}`);
})();

// optional chaining
(function () {
  const o = { a: { b: () => 'called', c: null }, arr: [1, 2] };
  log('opt', o?.a?.b(), o.x?.y.z, o.a.c?.d, o.a.missing?.(), o.arr?.[1], o.nope?.[0]);
  const n = null;
  log('opt2', n?.x, n?.[1], n?.f());
})();

// this, methods, classes
(function () {
  const o = {
    v: 42,
    m() { return this.v; },
    arrow() { return [1].map(() => this.v)[0]; },
    get g() { return this.v * 2; },
    set g(x) { this.v = x; },
    ['comp' + 'uted']: 'yes',
  };
  log('this', o.m(), o.arrow(), o.g, o.computed);
  o.g = 7;
  log('setter', o.v);
  class A { constructor(x) { this.x = x; } get dbl() { return this.x * 2; } static s() { return 'static'; } m() { return 'A' + this.x; } }
  class B extends A { constructor(x) { super(x + 1); } m() { return 'B' + super.m(); } }
  const b = new B(1);
  log('class', b.x, b.dbl, b.m(), B.s(), b instanceof A);
  function Ctor(v) { this.v = v; }
  Ctor.prototype.get = function () { return this.v; };
  log('proto', new Ctor(3).get());
  const sp = { ...{ a: 1, b: 2 }, c: 3 };
  log('spread', JSON.stringify(sp), Math.max(...[1, 5, 3]), [...'hi', ...[1, 2]].join());
  const f = function named() { return typeof named; };
  log('named-fe', f());
  const anon = function () {}; const arrowf = () => {};
  log('fn-names', anon.name, arrowf.name, o.m.name);
})();

// generators and iterators
(function () {
  function* g() { const x = yield 1; log('got', x); try { yield 2; } finally { log('gen-fin'); } yield 3; }
  const it = g();
  log('gen', JSON.stringify(it.next()), JSON.stringify(it.next('X')), JSON.stringify(it.return(9)), JSON.stringify(it.next()));
  function* fib() { let [a, b] = [0, 1]; for (;;) { yield a; [a, b] = [b, a + b]; } }
  const fs = []; for (const v of fib()) { if (v > 50) break; fs.push(v); }
  log('fib', fs.join());
  const iterable = { from: 1, to: 4, [Symbol.iterator]() { let c = this.from, t = this.to; return { next: () => c <= t ? { value: c++, done: false } : { value: undefined, done: true }, return() { log('iter-closed'); return {}; } }; } };
  for (const v of iterable) { if (v === 2) break; }
  log('iter', [...iterable].join());
})();

// async
(function () {
  async function a(x) { const v = await Promise.resolve(x); return v * 2; }
  async function b() { try { await Promise.reject(new Error('rej')); } catch (e) { return 'caught ' + e.message; } }
  Promise.all([a(2), b()]).then(r => { log('async', r.join()); done(); });
})();

// recursion and misc
(function () {
  function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); }
  log('fact', fact(10));
  const memo = {}; function fibm(n) { if (n in memo) return memo[n]; return memo[n] = n < 2 ? n : fibm(n - 1) + fibm(n - 2); }
  log('fibm', fibm(50));
  log('regex', /a(b+)c/.exec('xabbbc')[1], 'a-b-c'.replace(/-/g, '+'));
  log('tagged', ((s, ...v) => s.raw.join('|') + v.join())`a${1}b${2}c`);
  var self = this;
  log('sloppy-this', self === globalThis || self === undefined);
  log('json', JSON.stringify({ a: [1, { b: 2 }], c: 'x' }));
  let total = 0; [1, 2, 3].forEach(function (v) { total += v; });
  log('forEach', total);
  const nested = () => () => () => 'deep';
  log('nested', nested()()());
  log('eval', eval('1 + 2'));
  with_test: { log('label-ok'); }
})();

function done() { console.log(out.join('\n')); }
