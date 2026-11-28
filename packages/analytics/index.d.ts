export type AllowedPropertyValues = string | number | boolean | null | undefined

export type BeforeSendEvent = { type: 'pageview' | 'event'; url: string }
export type BeforeSend = (event: BeforeSendEvent) => BeforeSendEvent | null

export type AnalyticsProps = {
  /** `development` prints what would be sent instead of sending it. Default: `production`, or `development` under NODE_ENV=development. */
  mode?: 'auto' | 'development' | 'production'
  debug?: boolean
  beforeSend?: BeforeSend
  /** Page views only through `pageview()`. */
  disableAutoTrack?: boolean
  /** Where the script is served, if not at /_forkrail/insights/script.js. */
  scriptSrc?: string
  /** Where it sends to, if not next to the script. */
  endpoint?: string
  /** The app's base path, if it has one. */
  basePath?: string
  /** Named in what's sent, after the package's name. */
  framework?: string
}

export declare function inject(props?: AnalyticsProps): void
export declare function track(name: string, properties?: Record<string, AllowedPropertyValues>, options?: { flags?: string[] | Record<string, AllowedPropertyValues> }): void
export declare function pageview(page?: { route?: string | null; path?: string }): void
export declare function computeRoute(pathname: string | null, params: Record<string, string | string[]> | null): string | null

declare const _default: { inject: typeof inject; track: typeof track; pageview: typeof pageview; computeRoute: typeof computeRoute }
export default _default
