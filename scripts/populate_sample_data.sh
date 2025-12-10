#!/bin/bash
PORT=8000
API_URL="http://localhost:$PORT/api/books"

echo "Populating database on port $PORT..."

# 1. Dune - Reading
echo "Adding Dune..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Dune",
  "author": "Frank Herbert",
  "isbn": "9780441013593",
  "publisher": "Ace",
  "publication_year": 1965,
  "summary": "Set on the desert planet Arrakis, Dune is the story of the boy Paul Atreides...",
  "reading_status": "reading",
  "started_reading_at": "2023-11-01T10:00:00Z"
}'
echo ""

# 2. Project Hail Mary - To Read
echo "Adding Project Hail Mary..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Project Hail Mary",
  "author": "Andy Weir",
  "isbn": "9780593135204",
  "publisher": "Ballantine Books",
  "publication_year": 2021,
  "summary": "Ryland Grace is the sole survivor on a desperate, last-chance mission...",
  "reading_status": "to_read"
}'
echo ""

# 3. The Hobbit - Read
echo "Adding The Hobbit..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "The Hobbit",
  "author": "J.R.R. Tolkien",
  "isbn": "9780547928227",
  "publisher": "Houghton Mifflin",
  "publication_year": 1937,
  "summary": "In a hole in the ground there lived a hobbit...",
  "reading_status": "read",
  "started_reading_at": "2023-01-15T09:00:00Z",
  "finished_reading_at": "2023-02-10T18:00:00Z"
}'
echo ""

# 4. Neuromancer - Wanted
echo "Adding Neuromancer..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Neuromancer",
  "author": "William Gibson",
  "isbn": "9780441569595",
  "reading_status": "wanted"
}'
echo ""

# 5. Unknown Book (No ISBN, test fallback cover)
echo "Adding The Anthology of Unknown Things..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "The Anthology of Unknown Things",
  "author": "Anonymous",
  "summary": "A collection of mysterious tales.",
  "reading_status": "to_read"
}'
echo ""

# 6. Rust for Rustaceans - Reading
echo "Adding Rust for Rustaceans..."
curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Rust for Rustaceans",
  "author": "Jon Gjengset",
  "isbn": "9781718501850",
  "reading_status": "reading",
  "started_reading_at": "2023-10-01T12:00:00Z"
}'
echo ""

echo "Done."
