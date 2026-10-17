// Closures, array methods.
var t = Date.now(), a = [];
for (var i = 0; i < 200000; i++) a.push(i);
var s = 0;
for (var k = 0; k < 5; k++) s += a.map(function (x) { return x * 2; }).filter(function (x) { return x % 3 === 0; }).reduce(function (p, c) { return p + c; }, 0);
console.log('closures', Date.now() - t, 'ms', s);
