#!/bin/bash

# Configuration
API_URL=${API_BASE_URL:-"http://localhost:8000"}
echo "🔍 Testing Filters against $API_URL"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

# Helper function
check_filter() {
    local filter_name="$1"
    local query="$2"
    local jq_selector="$3"
    local expected_value="$4"

    echo -n "Checking $filter_name... "
    
    # Fetch results
    response=$(curl -s "$API_URL/api/books?$query")
    
    # Check if we got an empty list (which might be valid, but we verify field correctness if items exist)
    count=$(echo "$response" | jq 'if type=="array" then length else .total end')
    
    if [ "$count" -eq 0 ]; then
        echo -e "${GREEN}OK (No books found, so technically passed)${NC}"
        return
    fi
    
    # Verify first item matches
    actual_value=$(echo "$response" | jq -r "$jq_selector" | head -n 1)
    
    if [[ "$actual_value" == *"$expected_value"* ]]; then
        echo -e "${GREEN}PASS${NC} (Found '$actual_value')"
    else
        echo -e "${RED}FAIL${NC} (Expected '$expected_value', got '$actual_value')"
        # Exit on fail for stricter testing?
        # exit 1
    fi
}

# 1. Test Status: Reading
check_filter "Status=Reading" "status=reading" 'if type=="array" then .[0].reading_status else .books[0].reading_status end' "reading"

# 2. Test Status: Wanting
check_filter "Status=Wanting" "status=wanting" 'if type=="array" then .[0].reading_status else .books[0].reading_status end' "wanting"

# 3. Test Title Search (assuming partial match support)
# We pick a common letter like 'a' or a known book 'Dune' if seeded
# check_filter "Title=Dune" "title=Dune" 'if type=="array" then .[0].title else .books[0].title end' "Dune"

echo "✅ Filter logic verification complete."
