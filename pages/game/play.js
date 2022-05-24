// Create WebSocket connection.
const socket = new WebSocket('wss://ws.postman-echo.com/raw');

// Connection opened
socket.addEventListener('open', function (event) {
	socket.send('Hello Server');
});

//  Listen for messages
socket.addEventListener('message',
function(event) {
	console.log('Message from server: ', event.data);
});
