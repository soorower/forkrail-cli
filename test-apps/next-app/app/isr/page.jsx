export const revalidate = 30

export default function Isr() {
  return (
    <main>
      <h1>ISR</h1>
      <p id="built">{new Date().toISOString()}</p>
    </main>
  )
}
