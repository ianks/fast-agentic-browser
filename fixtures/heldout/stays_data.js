// Shared data for the Skerry Stays fixture (stays.html + stays_hotel.html).
window.SKERRY = {
  hotels: [
    { id: 'h1', name: 'Fjordlys Hotel', area: 'Svolvær centre', rating: 9.1, reviews: 812, rooms: [
      { id: 'r1', name: 'Standard double', price: 142, cancel: 'Non-refundable', breakfast: false },
      { id: 'r2', name: 'Standard double — flexible', price: 168, cancel: 'Free cancellation until 12 Oct', breakfast: true } ] },
    { id: 'h2', name: 'Havbris Guesthouse', area: 'Kabelvåg', rating: 8.4, reviews: 233, rooms: [
      { id: 'r1', name: 'Double room', price: 98, cancel: 'Free cancellation until 13 Oct', breakfast: true } ] },
    { id: 'h3', name: 'Nordlys Harbour Inn', area: 'Svolvær harbour', rating: 8.7, reviews: 540, rooms: [
      { id: 'r1', name: 'Double room', price: 131, cancel: 'Free cancellation until 13 Oct', breakfast: false },
      { id: 'r2', name: 'Double room with breakfast', price: 154, cancel: 'Non-refundable', breakfast: true },
      { id: 'r3', name: 'Double room with breakfast — flexible', price: 159, cancel: 'Free cancellation until 13 Oct', breakfast: true } ] },
    { id: 'h4', name: 'Skarven Rorbuer', area: 'Svolvær waterfront', rating: 9.4, reviews: 1204, rooms: [
      { id: 'r1', name: 'Fisherman\'s cabin', price: 189, cancel: 'Free cancellation until 11 Oct', breakfast: true } ] },
    { id: 'h5', name: 'Polar Pier Hotel', area: 'Svolvær harbour', rating: 8.9, reviews: 677, rooms: [
      { id: 'r1', name: 'Economy double', price: 129, cancel: 'Non-refundable', breakfast: true },
      { id: 'r2', name: 'Double room — flexible', price: 149, cancel: 'Free cancellation until 13 Oct', breakfast: true },
      { id: 'r3', name: 'Superior double — flexible', price: 176, cancel: 'Free cancellation until 13 Oct', breakfast: true } ] },
    { id: 'h6', name: 'Polarlys Pier Hostel', area: 'Svolvær harbour', rating: 8.6, reviews: 398, rooms: [
      { id: 'r1', name: 'Private double', price: 119, cancel: 'Partially refundable (50% until 13 Oct)', breakfast: true } ] },
    { id: 'h7', name: 'Lysverket Suites', area: 'Svolvær centre', rating: 8.5, reviews: 145, rooms: [
      { id: 'r1', name: 'Studio', price: 152, cancel: 'Free cancellation until 12 Oct', breakfast: true } ] },
    { id: 'h8', name: 'Tindholmen Lodge', area: 'Kleppstad', rating: 9.0, reviews: 301, rooms: [
      { id: 'r1', name: 'Lodge room', price: 160, cancel: 'Free cancellation until 13 Oct', breakfast: true },
      { id: 'r2', name: 'Lodge room — room only', price: 139, cancel: 'Free cancellation until 13 Oct', breakfast: false } ] },
  ],
  dates: [['2026-10-12', 'Mon 12 Oct'], ['2026-10-13', 'Tue 13 Oct'], ['2026-10-14', 'Wed 14 Oct'], ['2026-10-15', 'Thu 15 Oct'], ['2026-10-16', 'Fri 16 Oct'], ['2026-10-17', 'Sat 17 Oct'], ['2026-10-18', 'Sun 18 Oct'], ['2026-10-19', 'Mon 19 Oct']],
  defaults: { checkin: '2026-10-16', checkout: '2026-10-18', adults: 2 },
  load() { try { return Object.assign({}, this.defaults, JSON.parse(sessionStorage.getItem('skerry.search') || '{}')); } catch (e) { return Object.assign({}, this.defaults); } },
  save(s) { sessionStorage.setItem('skerry.search', JSON.stringify(s)); },
  nights(s) { return Math.round((Date.parse(s.checkout) - Date.parse(s.checkin)) / 86400000); },
  label(d) { const x = this.dates.find((y) => y[0] === d); return x ? x[1] : d; },
};
