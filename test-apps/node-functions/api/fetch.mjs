// export default { fetch }: every method goes to fetch.
export default {
  async fetch(request) {
    return new Response(`${request.method} ${new URL(request.url).search}`, { headers: { 'x-shape': 'fetch' } });
  },
};
