// <Analytics /> for Vue: inject() once when it mounts. Page views follow the address, client-side
// navigations included.
import { defineComponent, onMounted } from 'vue'
import { inject } from './index.js'

export { track } from './index.js'

export const Analytics = defineComponent({
  name: 'Analytics',
  props: ['mode', 'debug', 'disableAutoTrack', 'scriptSrc', 'endpoint', 'basePath', 'beforeSend'],
  setup(props) {
    onMounted(() => {
      const given = Object.fromEntries(Object.entries(props).filter(([, v]) => v !== undefined))
      inject({ framework: 'vue', ...given })
    })
    return () => null
  },
})

export default Analytics
