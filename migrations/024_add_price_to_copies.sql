-- Add price column to copies table for bookseller profile
-- The price is optional and overrides the book's default price when set
-- If NULL, the price from the parent book is used

ALTER TABLE copies ADD COLUMN price REAL;
