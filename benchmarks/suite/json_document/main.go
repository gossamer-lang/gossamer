package main

import (
	"encoding/json"
	"fmt"
	"strings"
)

type Geo struct {
	Lat float64 `json:"lat"`
	Lng float64 `json:"lng"`
}

type Address struct {
	City string `json:"city"`
	Zip  string `json:"zip"`
	Geo  Geo    `json:"geo"`
}

type Record struct {
	ID      int64    `json:"id"`
	Name    string   `json:"name"`
	Active  bool     `json:"active"`
	Score   float64  `json:"score"`
	Tags    []string `json:"tags"`
	Address Address  `json:"address"`
}

type Doc struct {
	Records []Record `json:"records"`
}

func document(n int) string {
	var b strings.Builder
	b.Grow(n * 160)
	b.WriteString(`{"records":[`)
	for i := 0; i < n; i++ {
		if i > 0 {
			b.WriteString(",")
		}
		fmt.Fprintf(&b, `{"id":%d,"name":"user%d","active":%t,"score":%d.5,"tags":["a","b"],"address":{"city":"London","zip":"%d","geo":{"lat":51.5,"lng":-0.12}}}`,
			i, i, i%3 == 0, i%1000, i)
	}
	b.WriteString("]}")
	return b.String()
}

func main() {
	text := []byte(document(40_000))
	var doc map[string]any
	if err := json.Unmarshal(text, &doc); err != nil {
		panic(err)
	}
	records := doc["records"].([]any)
	active, lat := 0, 0.0
	for _, r := range records {
		record := r.(map[string]any)
		if record["active"].(bool) {
			active++
		}
		lat += record["address"].(map[string]any)["geo"].(map[string]any)["lat"].(float64)
	}
	var typed Doc
	if err := json.Unmarshal(text, &typed); err != nil {
		panic(err)
	}
	var ids int64
	for _, record := range typed.Records {
		ids += record.ID
	}
	fmt.Printf("%d %d %.1f %d\n", len(records), active, lat, ids)
}
