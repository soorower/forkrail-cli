// A dynamic segment: Vercel puts it in the query as `id`.
package users

import (
	"fmt"
	"net/http"
)

func User(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("content-type", "application/json")
	fmt.Fprintf(w, `{"function":"api/users/[id].go","id":%q}`, r.URL.Query().Get("id"))
}
