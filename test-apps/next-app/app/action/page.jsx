import { revalidatePath } from 'next/cache'

let count = 0

async function bump() {
  'use server'
  count += 1
  revalidatePath('/action')
}

export const dynamic = 'force-dynamic'

export default function Action() {
  return (
    <main>
      <h1>Server action</h1>
      <p id="count">{count}</p>
      <form action={bump}>
        <button type="submit">Bump</button>
      </form>
    </main>
  )
}
