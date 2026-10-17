// Arithmetic in a hot loop.
var t = Date.now(), s = 0;
for (var i = 0; i < 3000000; i++) { s = (s + i * 3) % 1000003; }
console.log('loop', Date.now() - t, 'ms', s);
