// An Express app written in TypeScript that exports its app rather than listening, which Vercel's
// Express preset takes: forkrail compiles it with esbuild and serves the export.
import express, { type Request, type Response } from 'express'
import { greeting } from './greeting'

const app = express()
app.use(express.json())

app.get('/', (req: Request, res: Response) => {
  res.json({ framework: 'express', node: process.version, message: greeting(req.query.name as string | undefined) })
})

app.post('/echo', (req: Request, res: Response) => {
  console.log(`echo ${JSON.stringify(req.body).length} bytes`)
  res.json({ body: req.body })
})

app.get('/items/:id', (req: Request, res: Response) => {
  res.json({ id: req.params.id })
})

export default app
