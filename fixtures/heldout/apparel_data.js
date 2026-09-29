// Shared catalog + bag for the Marlowe & Pine fixture (apparel.html, apparel_item.html).
window.MP = {
  products: [
    { id: 'hebden', name: 'Hebden Waxed Jacket', price: 245, colors: { Olive: { S: 3, M: 0, L: 2, XL: 1 }, Navy: { S: 0, M: 4, L: 5, XL: 2 }, Rust: { S: 1, M: 2, L: 0, XL: 0 } }, desc: 'Waxed cotton with a corduroy collar and a tartan lining.' },
    { id: 'hebden-lite', name: 'Hebden Lite Waxed Jacket', price: 189, colors: { Olive: { S: 2, M: 6, L: 3, XL: 1 }, Sand: { S: 1, M: 1, L: 2, XL: 0 } }, desc: 'A lighter, unlined version of our best-selling jacket.' },
    { id: 'hebden-gilet', name: 'Hebden Waxed Gilet', price: 129, colors: { Olive: { S: 4, M: 5, L: 2, XL: 2 } }, desc: 'Sleeveless layer in the same waxed cotton.' },
    { id: 'calder', name: 'Calder Quilted Jacket', price: 165, colors: { Olive: { S: 1, M: 3, L: 4, XL: 2 }, Black: { S: 2, M: 2, L: 1, XL: 1 } }, desc: 'Diamond-quilted with a cord collar.' },
    { id: 'ribble', name: 'Ribble Fisherman Jumper', price: 98, colors: { Ecru: { S: 5, M: 5, L: 5, XL: 3 }, Navy: { S: 2, M: 0, L: 3, XL: 1 } }, desc: 'Heavy-gauge wool knit.' },
    { id: 'wharfe', name: 'Wharfe Wax Cap', price: 39, colors: { Olive: { 'One size': 12 } }, desc: 'Matches the Hebden jacket.' },
  ],
  bag() { try { return JSON.parse(sessionStorage.getItem('mp.bag') || '[]'); } catch (e) { return []; } },
  setBag(b) { sessionStorage.setItem('mp.bag', JSON.stringify(b)); },
  header() {
    const n = this.bag().reduce((a, x) => a + x.qty, 0);
    return `<header><a href="apparel.html" style="color:#fff;font-weight:800;letter-spacing:.5px">MARLOWE &amp; PINE</a><a href="apparel.html">Outerwear</a><a href="apparel.html">Knitwear</a><a href="apparel.html">Accessories</a><span style="margin-left:auto">🛍 Bag (<span id="bagn">${n}</span>)</span></header>`;
  },
};
