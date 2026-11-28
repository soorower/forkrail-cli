import Link from 'next/link'

export default function Home() {
  return (
    <main>
      <h1>Static home</h1>
      <ul>
        <li><Link href="/ssr">SSR</Link></li>
        <li><Link href="/isr">ISR</Link></li>
        <li><Link href="/blog/first">Blog (prerendered)</Link></li>
        <li><Link href="/blog/other">Blog (on demand)</Link></li>
        <li><Link href="/action">Server action</Link></li>
        <li><Link href="/img">Image</Link></li>
      </ul>
    </main>
  )
}
