// A Go web server on PORT, as Vercel's Go preset runs it.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"runtime"
	"time"
)

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /{$}", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, map[string]string{"framework": "go", "go": runtime.Version(), "arch": runtime.GOARCH})
	})
	mux.HandleFunc("POST /echo", func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		fmt.Printf("echo %d bytes\n", len(body))
		w.Write(body)
	})
	mux.HandleFunc("GET /sleep", func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(200 * time.Millisecond)
		writeJSON(w, map[string]string{"slept": "200ms"})
	})
	mux.HandleFunc("GET /env/{name}", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, map[string]string{"name": r.PathValue("name"), "value": os.Getenv(r.PathValue("name"))})
	})
	port := os.Getenv("PORT")
	if port == "" {
		port = "3000"
	}
	log.Printf("listening on :%s", port)
	log.Fatal(http.ListenAndServe(":"+port, mux))
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("content-type", "application/json")
	json.NewEncoder(w).Encode(v)
}
