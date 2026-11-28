export async function getStaticProps() {
  return { props: { built: new Date().toISOString() }, revalidate: 30 }
}

export default function Isr({ built }) {
  return <h1>ISR {built}</h1>
}
