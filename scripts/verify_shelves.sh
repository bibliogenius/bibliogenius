#!/bin/bash
# Verify Shelves Endpoint

echo "Wait for backend..."
sleep 2

echo "Calling /api/books/tags..."
RESPONSE=$(curl -s http://localhost:8000/api/books/tags)

echo "Response: $RESPONSE"

if [[ $RESPONSE == *'"name":'* ]] && [[ $RESPONSE == *'"count":'* ]]; then
  echo "✅ Shelves API returned valid tags."
else
  echo "❌ Shelves API failed or returned empty/invalid response."
fi
