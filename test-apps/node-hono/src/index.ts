// A Hono app that exports itself (its `fetch`), as Vercel's Hono preset takes it.
import { Hono } from 'hono'

const app = new Hono()

app.get('/', (c) => c.json({ framework: 'hono', node: process.version }))
app.post('/echo', async (c) => {
  const body = await c.req.text()
  console.log(`echo ${body.length} bytes`)
  return c.text(body)
})
app.get('/cookies', (c) => {
  c.header('set-cookie', 'a=1; Path=/', { append: true })
  c.header('set-cookie', 'b=2; Path=/', { append: true })
  return c.text('two cookies')
})

export default app
