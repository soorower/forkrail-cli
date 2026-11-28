/* Forkrail Web Analytics. Counts this site's page views and events where the site itself runs,
   with no cookies and nothing sent anywhere but the site itself. It answers the calls
   @vercel/analytics and @forkrail/analytics make (window.va and its queue, window.vaq). */
(function () {
  "use strict";
  var w = window, d = document, loc = w.location;
  if (w.__forkrailInsights) return;
  w.__forkrailInsights = 1;

  var me = d.currentScript;
  var data = (me && me.dataset) || {};
  var base = data.endpoint || (me && me.src ? new URL(me.src, loc.href).pathname.replace(/\/script(\.debug)?\.js$/, "") : "/_vercel/insights");
  var viewUrl = data.viewEndpoint || base + "/view";
  var eventUrl = data.eventEndpoint || base + "/event";
  var auto = !data.disableAutoTrack;
  var sdkn = data.sdkn, sdkv = data.sdkv;
  // The packages set window.vam; 'development' prints instead of sending.
  var dev = w.vam === "development";
  var beforeSend = null, last = null, sent = 0, route;

  function page() {
    return loc.origin + loc.pathname + loc.search;
  }

  // beforeSend may change an event's URL, or return null to drop it.
  function screen(type, url) {
    var event = { type: type, url: url };
    if (beforeSend) {
      try {
        event = beforeSend(event);
      } catch (e) {
        return null;
      }
    }
    return event && event.url ? event : null;
  }

  function send(url, body) {
    body.ts = Date.now();
    if (sdkn) body.sdkn = sdkn;
    if (sdkv) body.sdkv = sdkv;
    if (dev) {
      if (data.debug !== "false") console.log("[Forkrail Web Analytics]", url, body);
      return;
    }
    var text = JSON.stringify(body);
    try {
      if (w.fetch) {
        // text/plain keeps it a simple request. It goes as the page's own requests do (same-origin
        // credentials), so the browser sends it on the page's connection: without them it opens one
        // of its own, and a network that gives each connection another address counts one person as
        // two. Nothing is set, and the router reads no cookie from it.
        w.fetch(url, { method: "POST", body: text, keepalive: true, credentials: "same-origin", headers: { "content-type": "text/plain;charset=UTF-8" } })["catch"](function () {});
      } else if (navigator.sendBeacon) {
        navigator.sendBeacon(url, text);
      }
    } catch (e) {}
  }

  function pageview(dp) {
    var key = loc.pathname + loc.search;
    if (key === last) return;
    last = key;
    route = dp || undefined;
    var event = screen("pageview", page());
    if (!event) return;
    // The referrer counts for the page the visitor arrived on, not for later navigations.
    send(viewUrl, { o: event.url, r: sent++ ? "" : d.referrer, dp: route });
  }

  // Flag values, given as an object, or as names whose values the Flags SDK put on the page.
  function flags(given) {
    if (!given) return undefined;
    if (!Array.isArray(given)) return { p: given };
    var known = {}, out = {}, any = false;
    var tags = d.querySelectorAll("script[data-flag-values]");
    for (var i = 0; i < tags.length; i++) {
      try {
        var values = JSON.parse(tags[i].textContent || "{}");
        for (var k in values) known[k] = values[k];
      } catch (e) {}
    }
    for (var j = 0; j < given.length; j++) {
      if (given[j] in known) {
        out[given[j]] = known[given[j]];
        any = true;
      }
    }
    return any ? { p: out } : undefined;
  }

  function track(name, properties, options) {
    if (!name) return;
    var event = screen("event", page());
    if (!event) return;
    send(eventUrl, { o: event.url, en: String(name), ed: properties || undefined, dp: route, f: flags(options && options.flags) });
  }

  function run(args) {
    var command = args[0], arg = args[1] || {};
    if (command === "beforeSend") beforeSend = typeof args[1] === "function" ? args[1] : null;
    else if (command === "event") track(arg.name, arg.data, arg.options);
    else if (command === "pageview") pageview(arg.route);
  }

  function start() {
    var queued = w.vaq || [];
    w.va = function () {
      run(Array.prototype.slice.call(arguments));
    };
    w.vaq = [];
    for (var i = 0; i < queued.length; i++) run(queued[i]);
    if (!auto) return;
    pageview();
    // Client-side navigations: history changes, after the router has moved.
    var later = function () {
      setTimeout(function () {
        pageview();
      });
    };
    ["pushState", "replaceState"].forEach(function (name) {
      var original = history[name];
      history[name] = function () {
        var result = original.apply(this, arguments);
        later();
        return result;
      };
    });
    w.addEventListener("popstate", later);
  }

  // A page the browser renders ahead of a visit isn't a view until it's shown.
  if (d.visibilityState === "prerender") {
    var shown = function () {
      if (d.visibilityState === "prerender") return;
      d.removeEventListener("visibilitychange", shown);
      start();
    };
    d.addEventListener("visibilitychange", shown);
  } else {
    start();
  }
})();
