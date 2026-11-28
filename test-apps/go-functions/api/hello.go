// A second function in the same package, whose handler has the same name: Vercel builds each file
// on its own, and so does forkrail's dispatcher.
package handler

import (
	"fmt"
	"io"
	"net/http"
)

func Handler(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodPost {
		body, _ := io.ReadAll(r.Body)
		fmt.Printf("hello got %d bytes\n", len(body))
		w.Write(body)
		return
	}
	name := r.URL.Query().Get("name")
	if name == "" {
		name = "world"
	}
	fmt.Fprintf(w, "hello, %s (from api/hello.go)", name)
}
