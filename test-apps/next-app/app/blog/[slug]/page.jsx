export const revalidate = 60
export const dynamicParams = true

export function generateStaticParams() {
  return [{ slug: 'first' }, { slug: 'second' }]
}

export default async function Post({ params }) {
  const { slug } = await params
  return (
    <main>
      <h1>Post {slug}</h1>
      <p id="built">{new Date().toISOString()}</p>
    </main>
  )
}
