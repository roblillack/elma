// Present OAuth2 dialogs to the user.
package oauthdialog

import (
	"errors"
	"net"
	"net/http"

	"github.com/skratchdot/open-golang/open"
	"golang.org/x/oauth2"
)

// OAuth2 errors defined in RFC 6749 section 4.1.2.1.
var (
	ErrInvalidRequest = errors.New("Invalid request")
	ErrUnauthorizedClient = errors.New("Client not authorized")
	ErrAccessDenied = errors.New("Access denied")
	ErrUnsupportedResponseType = errors.New("Unsupported response type")
	ErrInvalidScope = errors.New("Invalid scope")
	ErrServerError = errors.New("Server error")
	ErrTemporarilyUnavailable = errors.New("Temporarily unavailable")
)

var errorsByName = map[string]error{
	"invalid_request": ErrInvalidRequest,
	"unauthorized_client": ErrUnauthorizedClient,
	"access_denied": ErrAccessDenied,
	"unsupported_response_type": ErrUnsupportedResponseType,
	"invalid_scope": ErrInvalidScope,
	"server_error": ErrServerError,
	"temporarily_unavailable": ErrTemporarilyUnavailable,
}

type handlerResponse struct {
	State string

	Code string
	Error string
}

func defaultSuccessHandler(w http.ResponseWriter, req *http.Request) {
	w.Header().Set("Content-Type", "text/html")
	w.Write([]byte("You can close this window."))
}

// An OAuth2 dialog.
type Dialog struct {
	// If a value is sent to this channel, the dialog is cancelled.
	Cancel chan bool
	// HTTP handler called when user after user authorization.
	SuccessHandler http.HandlerFunc

	config *oauth2.Config
	done chan *handlerResponse
}

// Open the dialog.
func (d *Dialog) Open() (code string, err error) {
	// Start local HTTP server
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return
	}

	d.done = make(chan *handlerResponse)
	defer close(d.done)

	s := &http.Server{Handler: http.HandlerFunc(d.serveHTTP)}
	go s.Serve(ln)
	defer ln.Close()

	conf := d.config
	conf.RedirectURL = "http://" + ln.Addr().String()

	state, err := generateState()
	if err != nil {
		return
	}

	url := conf.AuthCodeURL(state)
	if err = open.Run(url); err != nil {
		return
	}

	select {
	case res := <-d.done:
		if res.State != state {
			err = errors.New("Invalid state supplied to RedirectURL")
			return
		}

		if res.Error != "" {
			var ok bool
			if err, ok = errorsByName[res.Error]; ok {
				return
			}

			err = errors.New(res.Error)
			return
		}

		code = res.Code
		return
	case <-d.Cancel:
		return
	}
}

func (d *Dialog) serveHTTP(w http.ResponseWriter, req *http.Request) {
	q := req.URL.Query()

	res := &handlerResponse{
		State: q.Get("state"),
		Code: q.Get("code"),
		Error: q.Get("error"),
	}

	if res.State == "" || (res.Code == "" && res.Error == "") {
		w.WriteHeader(http.StatusNotFound)
		return
	}

	d.done <- res

	if d.SuccessHandler != nil {
		d.SuccessHandler(w, req)
	}
}

// Create a new OAuth2 dialog.
func New(conf *oauth2.Config) *Dialog {
	return &Dialog{
		Cancel: make(chan bool),
		SuccessHandler: defaultSuccessHandler,
		config: conf,
	}
}

// Create a new OAuth2 dialog and open it.
func Open(conf *oauth2.Config) (code string, err error) {
	d := New(conf)
	return d.Open()
}
