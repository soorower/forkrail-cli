// Vercel's zero-config Go layout: each file in api/ is a function with one exported handler.
package handler

import (
	"fmt"
	"net/http"
	"runtime"

	"github.com/soorower/forkrail-cli/test-apps/go-functions/api/_lib"
)

func Handler(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("content-type", "application/json")
	fmt.Fprintf(w, `{"function":"api/index.go","go":%q,"path":%q,"greeting":%q}`, runtime.Version(), r.URL.Path, lib.Greeting())
}
