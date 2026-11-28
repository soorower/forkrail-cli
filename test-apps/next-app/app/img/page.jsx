import Image from 'next/image'

export default function Img() {
  return (
    <main>
      <h1>Image</h1>
      <Image src="/dot.png" alt="dot" width={64} height={64} />
    </main>
  )
}
