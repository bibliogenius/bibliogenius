#!/bin/bash
PORT=8000
API_URL="http://localhost:$PORT/api/books"

echo "Creating a test book with start date..."
RESPONSE=$(curl -s -X POST "$API_URL" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Test Book",
  "started_reading_at": "2023-01-01T00:00:00Z",
  "reading_status": "reading"
}')

echo "Create Response: $RESPONSE"

# Extract ID using jq (assuming response structure {"message":..., "book": {"id": ...}})
ID=$(echo $RESPONSE | jq -r '.book.id')

if [ "$ID" == "null" ]; then
  echo "Failed to create book"
  exit 1
fi

echo "Created Book ID: $ID"

echo "Verifying initial state..."
# Get all books and find this one
curl -s "$API_URL" | jq ".books[] | select(.id == $ID) | {id, title, started_reading_at}"

echo "Clearing started_reading_at..."
curl -s -X PUT "$API_URL/$ID" \
  -H "Content-Type: application/json" \
  -d '{
  "title": "Test Book",
  "started_reading_at": null
}'

echo ""
echo "Verifying final state..."
curl -s "$API_URL" | jq ".books[] | select(.id == $ID) | {id, title, started_reading_at}"
