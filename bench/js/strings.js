// String building and methods.
var t = Date.now(), parts = [];
for (var i = 0; i < 100000; i++) parts.push('item' + i);
var j = parts.join(','), c = 0;
for (var k = 0; k < 100000; k++) if (parts[k].indexOf('9') >= 0) c++;
console.log('strings', Date.now() - t, 'ms', j.length, c);
