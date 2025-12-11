#!/bin/bash
API_URL="http://localhost:8005/api/books"

echo "1. Fetching ALL books..."
ALL_COUNT=$(curl -s "$API_URL" | jq '.books | length')
echo "Total books: $ALL_COUNT"

echo "2. Fetching books with status=reading..."
READING_COUNT=$(curl -s "$API_URL?status=reading" | jq '.books | length')
echo "Books with status=reading: $READING_COUNT"

echo "3. Fetching books with status=to_read..."
TO_READ_COUNT=$(curl -s "$API_URL?status=to_read" | jq '.books | length')
echo "Books with status=to_read: $TO_READ_COUNT"

# Verification
if [ "$READING_COUNT" -eq "$ALL_COUNT" ] && [ "$TO_READ_COUNT" -eq "$ALL_COUNT" ] && [ "$ALL_COUNT" -gt 0 ]; then
    echo "FAIL: Filters seem ignored (counts match total)"
    exit 1
elif [ "$READING_COUNT" -eq 0 ] && [ "$TO_READ_COUNT" -eq 0 ] && [ "$ALL_COUNT" -gt 0 ]; then
    echo "WARNING: Filters return 0 matches. Could be valid if no books have that status, but suspicious."
else
    echo "SUCCESS: Filters seem to affect the count."
fi
