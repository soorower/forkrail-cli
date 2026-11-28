export async function getServerSideProps() {
  return { props: { now: new Date().toISOString() } }
}

export default function Ssr({ now }) {
  return <h1>SSR {now}</h1>
}
