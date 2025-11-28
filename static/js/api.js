// API Client for BiblioGenius
const API_BASE = '/api';

class API {
    async request(endpoint, options = {}) {
        const url = `${API_BASE}${endpoint}`;
        const response = await fetch(url, {
            headers: {
                'Content-Type': 'application/json',
                ...options.headers
            },
            ...options
        });

        if (!response.ok) {
            throw new Error(`API Error: ${response.statusText}`);
        }

        return response.json();
    }

    // Books
    async getBooks() {
        return this.request('/books');
    }

    async createBook(data) {
        return this.request('/books', {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async listRequests() {
        return this.request('/peers/requests');
    }

    async listOutgoingRequests() {
        return this.request('/peers/requests/outgoing');
    }

    async updateRequestStatus(id, status) {
        return this.request(`/peers/requests/${id}`, {
            method: 'PUT',
            body: JSON.stringify({ status })
        });
    }

    async requestBook(peerId, isbn, title) {
        return this.request(`/peers/${peerId}/request`, {
            method: 'POST',
            body: JSON.stringify({ book_isbn: isbn, book_title: title })
        });
    }

    async deleteBook(id) {
        return this.request(`/books/${id}`, { method: 'DELETE' });
    }

    // Copies
    async getCopies() {
        return this.request('/copies');
    }

    async getBookCopies(bookId) {
        return this.request(`/books/${bookId}/copies`);
    }

    async createCopy(data) {
        return this.request('/copies', {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async deleteCopy(id) {
        return this.request(`/copies/${id}`, { method: 'DELETE' });
    }

    // Contacts
    async getContacts(params = {}) {
        const query = new URLSearchParams(params).toString();
        return this.request(`/contacts${query ? '?' + query : ''}`);
    }

    async createContact(data) {
        return this.request('/contacts', {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async deleteContact(id) {
        return this.request(`/contacts/${id}`, { method: 'DELETE' });
    }

    // Loans
    async getLoans(params = {}) {
        const query = new URLSearchParams(params).toString();
        return this.request(`/loans${query ? '?' + query : ''}`);
    }

    async createLoan(data) {
        return this.request('/loans', {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async returnLoan(id) {
        return this.request(`/loans/${id}/return`, { method: 'PUT' });
    }

    // P2P
    async connectPeer(data) {
        return this.request('/peers/connect', {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async searchRemote(peerId, query) {
        const res = await fetch('/api/peers/proxy_search', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ peer_id: peerId, query })
        });
        if (!res.ok) throw new Error('Search failed');
        return res.json();
    }

    async sendRequest(peerId, bookIsbn, bookTitle) {
        const res = await fetch(`/api/peers/${peerId}/request`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ book_isbn: bookIsbn, book_title: bookTitle })
        });
        if (!res.ok) throw new Error('Request failed');
        return res.json();
    }

    async listRequests() {
        const res = await fetch('/api/peers/requests');
        if (!res.ok) throw new Error('Failed to fetch requests');
        return res.json();
    }

    async updateRequestStatus(id, status) {
        const res = await fetch(`/api/peers/requests/${id}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ status })
        });
        if (!res.ok) throw new Error('Update failed');
        return res.json();
    }

    async scanImage(file) {
        const formData = new FormData();
        formData.append('file', file);

        const res = await fetch('/api/scan/image', {
            method: 'POST',
            body: formData // Content-Type header is auto-set for FormData
        });

        if (!res.ok) throw new Error('OCR failed');
        return res.json();
    }
}

const api = new API();
