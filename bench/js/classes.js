// Classes, methods, `this`.
class V { constructor(x, y) { this.x = x; this.y = y; } add(o) { return new V(this.x + o.x, this.y + o.y); } len() { return Math.sqrt(this.x * this.x + this.y * this.y); } }
var t = Date.now(), v = new V(0, 0), d = new V(1, 2);
for (var i = 0; i < 300000; i++) v = v.add(d);
console.log('classes', Date.now() - t, 'ms', Math.round(v.len()));
