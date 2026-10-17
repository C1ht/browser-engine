// Object property reads/writes.
var t = Date.now(), o = { x: 1, y: 2, z: 3 }, s = 0;
for (var i = 0; i < 1000000; i++) { o.x = o.y + o.z; s += o.x; o.y = i & 7; }
console.log('props', Date.now() - t, 'ms', s);
