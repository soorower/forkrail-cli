// The Web shape: a handler per method, given a Request, returning a Response (streamed here).
export function GET(request: Request) {
  const url = new URL(request.url);
  const stream = new ReadableStream({
    async start(controller) {
      for (const part of ['streamed ', 'from ', url.pathname, '\n']) {
        controller.enqueue(new TextEncoder().encode(part));
        await new Promise((r) => setTimeout(r, 50));
      }
      controller.close();
    },
  });
  return new Response(stream, { headers: { 'content-type': 'text/plain; charset=utf-8', 'x-shape': 'web' } });
}

export async function POST(request: Request) {
  const body = await request.json();
  return Response.json({ got: body, method: 'POST' }, { status: 201 });
}
