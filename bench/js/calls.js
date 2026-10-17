// Function calls and recursion.
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
var t = Date.now();
var r = fib(25);
console.log('calls', Date.now() - t, 'ms', r);
