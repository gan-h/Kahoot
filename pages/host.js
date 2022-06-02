// Create WebSocket connection.
const socket = new WebSocket("ws://64.225.12.53/ws");
// buttons
const createRmBtn = document.getElementById("createRmBtn");
// rm id
const display = document.getElementById("display");
// name display
const names = document.getElementById("names");
// tracks score of player
const scores = new Map();
//  Listen for messages
var receiveMessage = (e) => {
	console.log("Message from server: ", event.data);
	let data = JSON.parse(event.data);
	console.log(data);

	switch (data.type) {
		case "roomCreated":
			display.innerText = `Room ID: ${data.roomId}`;
			break;
		case "userJoined":
			// adds child to hashmap
			scores.set(data.username, 0);
			console.log(scores);
			// adds child html element
			let child = document.createElement("div");
			child.classList.add("small-12");
			child.classList.add("medium-4");
			child.classList.add("large-2");
			child.classList.add("nameDisplay");

			child.setAttribute("id", `name-${data.username}`);

			child.innerText = data.username;

			names.appendChild(child);
			break;
		case "userLeft":
			// removes score tracking
			scores.delete(data.username);
			console.log(scores);
			// removes html element
			let left = document.getElementById(`name-${data.username}`);
			left.remove();
			break;
	}
};

socket.addEventListener("message", receiveMessage);

var createRoom = (questions) => {
	let request = {
		type: "createRoom",
		questions: questions
	};
	request = JSON.stringify(request);
	console.log("Creating room: ", request);
	socket.send(request);

	createRmBtn.remove();
}
