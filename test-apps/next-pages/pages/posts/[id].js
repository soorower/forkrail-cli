export async function getStaticPaths() {
  return { paths: [{ params: { id: '1' } }], fallback: 'blocking' }
}

export async function getStaticProps({ params }) {
  return { props: { id: params.id }, revalidate: 60 }
}

export default function Post({ id }) {
  return <h1>Post {id}</h1>
}
