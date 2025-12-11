#!/bin/bash
BASE_URL="http://localhost:8000"

# 1. Create a book with tags
echo "Creating book with tags..."
CREATE_RES=$(curl -s -X POST "$BASE_URL/api/books" \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Tag Test Book",
    "isbn": "9999999999",
    "subjects": ["Sci-Fi", "Testing"]
  }')

BOOK_ID=$(echo $CREATE_RES | jq -r '.id')
echo "Created Book ID: $BOOK_ID"

# 2. Fetch the book and check tags
echo "Fetching book..."
GET_RES=$(curl -s "$BASE_URL/api/books/$BOOK_ID")
TAGS=$(echo $GET_RES | jq -r '.subjects')
echo "Tags from API: $TAGS"

if [[ "$TAGS" == *"Sci-Fi"* && "$TAGS" == *"Testing"* ]]; then
  echo "✅ Tags successfully retrieved."
else
  echo "❌ Tags MISSING or incorrect."
fi

# 3. Update tags
echo "Updating tags..."
UPDATE_RES=$(curl -s -X PUT "$BASE_URL/api/books/$BOOK_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Tag Test Book Updated",
    "subjects": ["UpdatedTag", "NewTag"]
  }')

# 4. Fetch again
GET_RES_2=$(curl -s "$BASE_URL/api/books/$BOOK_ID")
TAGS_2=$(echo $GET_RES_2 | jq -r '.subjects')
echo "Updated Tags from API: $TAGS_2"

if [[ "$TAGS_2" == *"UpdatedTag"* ]]; then
  echo "✅ Updated tags successfully retrieved."
else
  echo "❌ Updated tags MISSING."
fi

# Cleanup
curl -s -X DELETE "$BASE_URL/api/books/$BOOK_ID"
