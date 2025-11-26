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
        return this.request('/peers/proxy_search', {
            method: 'POST',
            body: JSON.stringify({ peer_id: peerId, query })
        });
    }
}

const api = new API();
