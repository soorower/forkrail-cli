export const config = { runtime: 'edge' };

// An Edge function's default export takes a Request.
export default function handler(request: Request) {
  return new Response(JSON.stringify({ edge: true, ua: request.headers.get('user-agent') }), { headers: { 'content-type': 'application/json' } });
}
